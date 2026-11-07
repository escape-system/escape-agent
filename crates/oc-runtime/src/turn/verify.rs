//! Turn-end verification guard for coding edits (Hermes Agent `verify_on_stop` port).
//!
//! Intercepts turn finish when code files were modified without a subsequent
//! passing test or build command, prompting the agent to run verification.

use std::fmt::Write as _;
use std::path::Path;

use oc_config::ConfigService;

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

const VERIFICATION_COMMAND_PATTERNS: &[&str] = &[
    "cargo test",
    "cargo check",
    "cargo clippy",
    "cargo build",
    "npm test",
    "npx test",
    "npm run test",
    "yarn test",
    "pnpm test",
    "pytest",
    "python -m unittest",
    "python -m pytest",
    "python test",
    "go test",
    "make test",
    "make check",
    "ctest",
    "mvn test",
    "gradle test",
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

/// Checks if a shell command looks like a test or build verification command.
#[must_use]
pub fn is_verification_command(command: &str) -> bool {
    let lower = command.to_lowercase();
    VERIFICATION_COMMAND_PATTERNS
        .iter()
        .any(|pattern| lower.contains(pattern))
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
#[must_use]
pub fn verify_on_stop_enabled(config: &ConfigService) -> bool {
    if let Ok(env) = std::env::var("OPENCODE_VERIFY_ON_STOP") {
        let trimmed = env.trim().to_lowercase();
        return trimmed == "1" || trimmed == "true" || trimmed == "yes" || trimmed == "on";
    }
    config
        .merged()
        .get("verify_on_stop")
        .and_then(serde_json::Value::as_bool)
        .or_else(|| {
            config
                .merged()
                .pointer("/agent/verify_on_stop")
                .and_then(serde_json::Value::as_bool)
        })
        .unwrap_or(false)
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
        assert!(is_verification_command("npm test"));
        assert!(is_verification_command("pytest tests/"));
        assert!(is_verification_command("python -m unittest discover"));
        assert!(is_verification_command("go test ./..."));

        assert!(!is_verification_command("ls -la"));
        assert!(!is_verification_command("git status"));
        assert!(!is_verification_command("echo hello"));
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
