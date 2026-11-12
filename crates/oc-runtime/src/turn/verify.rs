//! Turn-end verification guard for coding edits (Hermes Agent `verify_on_stop` port).
//!
//! Intercepts turn finish when code files were modified without a subsequent
//! passing test or build command, prompting the agent to run verification.
//! Enabled by default; disable with `verify_on_stop: false` (or
//! `agent.verify_on_stop: false`) or `OPENCODE_VERIFY_ON_STOP=0|false|no|off`.

use std::fmt::Write as _;
use std::path::Path;

use oc_config::ConfigService;
use serde_json::{Map, Value};

const NON_CODE_EXTENSIONS: &[&str] = &[
    "md", "markdown", "rst", "txt", "text", "log", "csv", "tsv", "png", "jpg", "jpeg", "gif",
    "svg", "ico", "lock",
];

const NON_CODE_FILENAMES: &[&str] = &[
    "license",
    "readme",
    "notice",
    "authors",
    "contributors",
    "changelog",
    "codeowners",
];

/// Returns true when `path` points to a code or configuration file that affects runtime behavior.
#[must_use]
pub fn is_code_file(path: &str) -> bool {
    let p = Path::new(path);
    if let Some(name) = p.file_name().and_then(|n| n.to_str()) {
        let lower = name.to_lowercase();
        if NON_CODE_FILENAMES.contains(&lower.as_str()) {
            return false;
        }
    }
    if let Some(ext) = p.extension().and_then(|e| e.to_str()) {
        let lower = ext.to_lowercase();
        if NON_CODE_EXTENSIONS.contains(&lower.as_str()) {
            return false;
        }
    }
    true
}

/// Checks whether a shell command executes a real test, check, lint, or build verification command
/// in any pipeline/command segment, ignoring non-executing references like `echo` or `grep`.
#[must_use]
pub fn is_verification_command(command: &str) -> bool {
    split_shell_segments(command)
        .into_iter()
        .any(|segment| segment_runs_verification(&segment))
}

fn segment_runs_verification(segment: &str) -> bool {
    let tokens = tokenize_shell(segment);
    let idx = strip_command_wrappers(&tokens);
    if idx >= tokens.len() {
        return false;
    }

    let raw_cmd = tokens[idx].to_ascii_lowercase();
    let cmd = Path::new(&raw_cmd)
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or(&raw_cmd);
    let args: Vec<String> = tokens[idx + 1..]
        .iter()
        .map(|s| s.to_ascii_lowercase())
        .collect();

    if matches!(cmd, "sh" | "bash" | "zsh" | "dash")
        && let Some(script) = extract_shell_c_script(&args)
    {
        return is_verification_command(script);
    }
    matches_verification_cmd(cmd, &args)
}

fn strip_command_wrappers(tokens: &[String]) -> usize {
    let mut idx = skip_env_assignments(tokens, 0);
    while idx < tokens.len() {
        let lower = tokens[idx].to_ascii_lowercase();
        let bin = Path::new(&lower)
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or(&lower);
        match bin {
            "command" | "time" | "nohup" => {
                idx += 1;
            }
            "env" => {
                idx += 1;
                while idx < tokens.len() {
                    let arg = &tokens[idx];
                    if arg == "-u" && idx + 1 < tokens.len() {
                        idx += 2;
                    } else if arg.starts_with('-') || is_env_assignment(arg) {
                        idx += 1;
                    } else {
                        break;
                    }
                }
            }
            "nice" => {
                idx += 1;
                if idx < tokens.len() && tokens[idx] == "-n" {
                    idx = (idx + 2).min(tokens.len());
                } else if idx < tokens.len() && tokens[idx].starts_with('-') {
                    idx += 1;
                }
            }
            "timeout" => {
                idx += 1;
                while idx < tokens.len() && tokens[idx].starts_with('-') {
                    let needs_val = matches!(
                        tokens[idx].as_str(),
                        "-k" | "-s" | "--kill-after" | "--signal"
                    );
                    idx += if needs_val && idx + 1 < tokens.len() {
                        2
                    } else {
                        1
                    };
                }
                if idx < tokens.len() {
                    idx += 1;
                }
            }
            _ => break,
        }
        idx = skip_env_assignments(tokens, idx);
    }
    idx
}

fn matches_verification_cmd(cmd: &str, args: &[String]) -> bool {
    match cmd {
        "pytest" | "py.test" | "vitest" | "jest" | "ctest" => true,
        "cargo" => {
            let sub = args
                .first()
                .filter(|a| a.starts_with('+'))
                .map_or_else(|| args.first(), |_| args.get(1));
            sub.is_some_and(|s| {
                matches!(
                    s.as_str(),
                    "test" | "check" | "clippy" | "build" | "nextest"
                )
            })
        }
        "npm" => match args.first().map(String::as_str) {
            Some("test" | "t") => true,
            Some("run") => args
                .get(1)
                .is_some_and(|s| s == "test" || s.starts_with("test:") || s == "check"),
            _ => false,
        },
        "npx" => args
            .first()
            .is_some_and(|s| matches!(s.as_str(), "test" | "vitest" | "jest" | "playwright")),
        "yarn" | "pnpm" | "bun" => match args.first().map(String::as_str) {
            Some("test") => true,
            Some("run") => args
                .get(1)
                .is_some_and(|s| s == "test" || s.starts_with("test:")),
            _ => false,
        },
        "python" | "python3" => matches!(
            (
                args.first().map(String::as_str),
                args.get(1).map(String::as_str)
            ),
            (Some("-m"), Some("pytest" | "unittest")) | (Some("test"), _)
        ),
        "uv" => {
            args.first().map(String::as_str) == Some("run")
                && args
                    .get(1)
                    .is_some_and(|s| matches!(s.as_str(), "pytest" | "py.test" | "unittest"))
        }
        "go" => args.first().map(String::as_str) == Some("test"),
        "make" | "gmake" => args
            .iter()
            .any(|a| !a.starts_with('-') && matches!(a.as_str(), "test" | "check")),
        "mvn" | "mvnw" | "gradle" | "gradlew" => args
            .iter()
            .any(|a| !a.starts_with('-') && matches!(a.as_str(), "test" | "check" | "verify")),
        _ => false,
    }
}

fn extract_shell_c_script(args: &[String]) -> Option<&str> {
    for (i, arg) in args.iter().enumerate() {
        let is_c = arg == "-c"
            || (arg.starts_with('-')
                && !arg.starts_with("--")
                && arg.chars().skip(1).all(char::is_alphabetic)
                && arg.contains('c'));
        if is_c {
            return args.get(i + 1).map(String::as_str);
        }
    }
    None
}

fn is_env_assignment(token: &str) -> bool {
    let Some((key, _)) = token.split_once('=') else {
        return false;
    };
    !key.is_empty()
        && key.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
        && !key.chars().next().is_some_and(|c| c.is_ascii_digit())
}

fn skip_env_assignments(tokens: &[String], mut idx: usize) -> usize {
    while idx < tokens.len() && is_env_assignment(&tokens[idx]) {
        idx += 1;
    }
    idx
}

fn split_shell_segments(command: &str) -> Vec<String> {
    let mut segments = Vec::new();
    let mut current = String::new();
    let mut chars = command.chars().peekable();
    let mut in_single = false;
    let mut in_double = false;
    let mut escaped = false;

    while let Some(ch) = chars.next() {
        if escaped {
            current.push(ch);
            escaped = false;
            continue;
        }
        if ch == '\\' && !in_single {
            current.push(ch);
            escaped = true;
            continue;
        }
        if ch == '\'' && !in_double {
            in_single = !in_single;
            current.push(ch);
            continue;
        }
        if ch == '"' && !in_single {
            in_double = !in_double;
            current.push(ch);
            continue;
        }
        if !in_single && !in_double {
            if ch == '#' && (current.is_empty() || current.ends_with(char::is_whitespace)) {
                for next in chars.by_ref() {
                    if next == '\n' {
                        segments.push(std::mem::take(&mut current));
                        break;
                    }
                }
                continue;
            }
            match ch {
                ';' | '\n' => {
                    segments.push(std::mem::take(&mut current));
                    continue;
                }
                '&' | '|' => {
                    if chars.peek() == Some(&ch) {
                        let _ = chars.next();
                    }
                    segments.push(std::mem::take(&mut current));
                    continue;
                }
                _ => {}
            }
        }
        current.push(ch);
    }
    if !current.trim().is_empty() {
        segments.push(current);
    }
    segments
}

fn tokenize_shell(segment: &str) -> Vec<String> {
    let mut tokens = Vec::new();
    let mut current = String::new();
    let mut in_token = false;
    let mut in_single = false;
    let mut in_double = false;
    let mut escaped = false;

    for ch in segment.chars() {
        if escaped {
            current.push(ch);
            in_token = true;
            escaped = false;
            continue;
        }
        if ch == '\\' && !in_single {
            escaped = true;
            in_token = true;
            continue;
        }
        if ch == '\'' && !in_double {
            in_single = !in_single;
            in_token = true;
            continue;
        }
        if ch == '"' && !in_single {
            in_double = !in_double;
            in_token = true;
            continue;
        }
        if ch.is_whitespace() && !in_single && !in_double {
            if in_token {
                tokens.push(std::mem::take(&mut current));
                in_token = false;
            }
            continue;
        }
        current.push(ch);
        in_token = true;
    }
    if in_token {
        tokens.push(current);
    }
    tokens
}

/// Returns a failure reason when completed `shell` metadata reports a timeout,
/// terminating signal, non-zero exit code, or failed status.
#[must_use]
pub(super) fn shell_failure_reason(metadata: Option<&Map<String, Value>>) -> Option<String> {
    let obj = metadata?;
    if obj.get("timeout").and_then(Value::as_bool) == Some(true) {
        return Some("shell command timed out".to_owned());
    }
    if let Some(signal) = obj.get("signal").and_then(Value::as_str) {
        return Some(format!("shell command killed by {signal}"));
    }
    if let Some(exit) = obj
        .get("exit")
        .or_else(|| obj.get("exitCode"))
        .and_then(Value::as_i64)
        && exit != 0
    {
        return Some(format!("shell command exited with code {exit}"));
    }
    if let Some(status) = obj.get("status").and_then(Value::as_str)
        && matches!(
            status,
            "failed" | "error" | "timeout" | "timed_out" | "cancelled"
        )
    {
        return Some(format!("shell command status: {status}"));
    }
    None
}

/// Formats the synthetic follow-up prompt when code was edited without fresh verification evidence.
#[must_use]
pub fn build_verification_nudge(changed_paths: &[String]) -> String {
    let mut files_list = String::new();
    for path in changed_paths.iter().take(5) {
        let _ = writeln!(files_list, "- `{path}`");
    }
    if changed_paths.len() > 5 {
        let _ = writeln!(files_list, "- ... (and {} more)", changed_paths.len() - 5);
    }

    format!(
        "[System: You edited code in this turn, but the workspace does not have fresh passing verification evidence yet.\n\
         \n\
         Changed code files:\n\
         {files_list}\n\
         Run the relevant verification command now (e.g. `cargo test`, `cargo check`, `npm test`, `pytest`), read any failure, repair the code, and summarize what passed. If verification is not possible, explain the concrete blocker instead of claiming the work is fully verified.]"
    )
}

/// Returns whether verify-on-stop is enabled via environment variable or configuration.
///
/// Defaults to `true`; disable via config (`verify_on_stop: false` /
/// `agent.verify_on_stop: false`) or `OPENCODE_VERIFY_ON_STOP=0|false|no|off`.
#[must_use]
pub fn verify_on_stop_enabled(config: &ConfigService) -> bool {
    if let Ok(env) = std::env::var("OPENCODE_VERIFY_ON_STOP") {
        let trimmed = env.trim().to_lowercase();
        if !trimmed.is_empty() {
            return trimmed == "1" || trimmed == "true" || trimmed == "yes" || trimmed == "on";
        }
    }
    config
        .merged()
        .get("verify_on_stop")
        .and_then(serde_json::Value::as_bool)
        .or_else(|| {
            config
                .merged()
                .pointer("/experimental/verify_on_stop")
                .and_then(serde_json::Value::as_bool)
        })
        .or_else(|| {
            config
                .merged()
                .pointer("/agent/verify_on_stop")
                .and_then(serde_json::Value::as_bool)
        })
        .unwrap_or(true)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classifies_code_files_accurately() {
        assert!(is_code_file("src/main.rs"));
        assert!(is_code_file("crates/oc-runtime/src/turn.rs"));
        assert!(is_code_file("package.json"));
        assert!(is_code_file("Cargo.toml"));
        assert!(is_code_file("app.py"));
        assert!(is_code_file("index.ts"));

        assert!(!is_code_file("README.md"));
        assert!(!is_code_file("docs/06-memory.md"));
        assert!(!is_code_file("LICENSE"));
        assert!(!is_code_file("test.log"));
        assert!(!is_code_file("data.csv"));
        assert!(!is_code_file("Cargo.lock"));
    }

    #[test]
    fn recognizes_verification_commands() {
        assert!(is_verification_command("cargo test --workspace"));
        assert!(is_verification_command("cargo check -p oc-runtime"));
        assert!(is_verification_command(
            "cd sub && RUST_BACKTRACE=1 timeout 30 cargo clippy --all-targets"
        ));
        assert!(is_verification_command("npm test"));
        assert!(is_verification_command("npm run test:unit"));
        assert!(is_verification_command("bun test src/foo.test.ts"));
        assert!(is_verification_command("pytest tests/"));
        assert!(is_verification_command("python -m unittest discover"));
        assert!(is_verification_command("go test ./..."));
        assert!(is_verification_command("sh -c 'cargo test --help'"));

        assert!(!is_verification_command("ls -la"));
        assert!(!is_verification_command("git status"));
        assert!(!is_verification_command("echo hello"));
        assert!(!is_verification_command("echo cargo test ok"));
        assert!(!is_verification_command("grep 'cargo test' README.md"));
        assert!(!is_verification_command("git commit -m \"fix cargo test\""));
        assert!(!is_verification_command("# cargo test"));
    }

    #[test]
    fn enabled_by_default_and_can_be_disabled() {
        let load = |body: &str| {
            let dir = tempfile::tempdir().unwrap();
            let home = dir.path().join("home");
            std::fs::create_dir_all(&home).unwrap();
            std::fs::write(dir.path().join("opencode.json"), body).unwrap();
            ConfigService::load(
                oc_config::LoadOptions::new(dir.path())
                    .with_env("HOME", home.to_string_lossy())
                    .with_env("XDG_CONFIG_HOME", home.join(".config").to_string_lossy()),
            )
            .unwrap()
        };

        assert!(verify_on_stop_enabled(&load("{}")));
        assert!(!verify_on_stop_enabled(&load(
            r#"{"verify_on_stop": false}"#
        )));
        assert!(!verify_on_stop_enabled(&load(
            r#"{"experimental": {"verify_on_stop": false}}"#
        )));
    }

    #[test]
    fn formats_nudge_with_paths() {
        let paths = vec!["src/main.rs".to_owned(), "src/lib.rs".to_owned()];
        let nudge = build_verification_nudge(&paths);
        assert!(nudge.contains("`src/main.rs`"));
        assert!(nudge.contains("`src/lib.rs`"));
        assert!(nudge.contains("verification evidence"));
    }
}
