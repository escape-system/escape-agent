//! Build-command classifier.
//!
//! The classifier never evaluates a shell. It splits the command string into
//! hard segments (`&&`, `||`, `|`, `;`, newline), parses each with
//! [`shell_words`], and returns **one** [`LaneRequest`] for the first segment
//! that names a build/test command. Chained commands are therefore not split
//! into several tickets here: the `shell` tool submits one ticket per command
//! string, and the lane covers the whole chain.
//!
//! Workspace resolution:
//!
//! - cargo/rustc: nearest ancestor `Cargo.toml` that declares
//!   `[workspace]`; otherwise the nearest `Cargo.toml` (package root);
//! - node package managers: nearest ancestor `package.json`;
//! - all other families: the canonical working directory.
//!
//! When no manifest is found the canonical working directory is used, so a
//! command run outside a project is still serialized rather than escaping the
//! lane. Paths are canonicalized before they become part of the lane key.

use std::path::{Path, PathBuf};

use crate::types::LaneRequest;

/// Classifies a shell command into a lane resource key.
///
/// Returns `None` when no segment names a supported build/test command.
pub fn classify(command: &str, cwd: &Path) -> Option<LaneRequest> {
    let family = split_segments(command)
        .into_iter()
        .find_map(|segment| parse_segment(segment).and_then(|tokens| classify_tokens(&tokens)))?;
    let root = match family {
        Family::Cargo => cargo_workspace(cwd),
        Family::Node => node_workspace(cwd),
        Family::Go | Family::Python | Family::Dotnet | Family::Gradle | Family::Maven => {
            canonical_or_self(cwd)
        }
    };
    Some(LaneRequest {
        lane: format!("{}:{}", family.prefix(), root.to_string_lossy()),
        command: command.to_owned(),
    })
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Family {
    Cargo,
    Node,
    Go,
    Python,
    Dotnet,
    Gradle,
    Maven,
}

impl Family {
    const fn prefix(self) -> &'static str {
        match self {
            Self::Cargo => "cargo",
            Self::Node => "node",
            Self::Go => "go",
            Self::Python => "python",
            Self::Dotnet => "dotnet",
            Self::Gradle => "gradle",
            Self::Maven => "maven",
        }
    }
}

const CARGO_SUBCOMMANDS: &[&str] = &["build", "test", "clippy", "nextest", "run", "check"];

fn parse_segment(segment: &str) -> Option<Vec<String>> {
    if segment.trim().is_empty() {
        return None;
    }
    shell_words::split(segment)
        .ok()
        .filter(|tokens| !tokens.is_empty())
}

fn classify_tokens(tokens: &[String]) -> Option<Family> {
    let index = skip_wrappers(tokens);
    let name = command_name(tokens.get(index)?);
    let args = tokens.get(index + 1..).unwrap_or_default();
    match name.as_str() {
        "cargo" => cargo_subcommand(args).then_some(Family::Cargo),
        "rustc" => Some(Family::Cargo),
        "npm" | "pnpm" | "bun" | "yarn" => node_command(args).then_some(Family::Node),
        "go" => go_test(args).then_some(Family::Go),
        "pytest" => Some(Family::Python),
        "python" | "python3" | "py" => python_pytest(args).then_some(Family::Python),
        "dotnet" => first_non_option(args)
            .is_some_and(|word| word == "test")
            .then_some(Family::Dotnet),
        "gradle" | "gradlew" => Some(Family::Gradle),
        "mvn" | "mvnw" => Some(Family::Maven),
        _ => None,
    }
}

fn cargo_subcommand(args: &[String]) -> bool {
    let mut index = 0;
    while let Some(token) = args.get(index) {
        if token.starts_with('+') {
            index += 1;
            continue;
        }
        if token.starts_with('-') {
            index += if !token.contains('=') && cargo_option_takes_value(token) {
                2
            } else {
                1
            };
            continue;
        }
        return CARGO_SUBCOMMANDS.contains(&token.as_str());
    }
    false
}

fn cargo_option_takes_value(option: &str) -> bool {
    matches!(
        option,
        "--manifest-path" | "--target-dir" | "--config" | "--color" | "--jobs" | "-j" | "-Z"
    )
}

fn node_command(args: &[String]) -> bool {
    let mut index = 0;
    while let Some(token) = args.get(index) {
        if token.starts_with('-') {
            index += if !token.contains('=') && node_option_takes_value(token) {
                2
            } else {
                1
            };
            continue;
        }
        if token == "run" || token == "run-script" {
            let mut script = index + 1;
            while args.get(script).is_some_and(|arg| arg.starts_with('-')) {
                script += 1;
            }
            return script < args.len();
        }
        return matches!(
            token.as_str(),
            "test" | "build" | "lint" | "check" | "typecheck"
        );
    }
    false
}

fn node_option_takes_value(option: &str) -> bool {
    matches!(
        option,
        "--prefix"
            | "--userconfig"
            | "--cache"
            | "--registry"
            | "--cwd"
            | "--dir"
            | "-C"
            | "--workspace"
            | "-w"
            | "--filter"
            | "-F"
            | "--reporter"
            | "--loglevel"
    )
}

fn go_test(args: &[String]) -> bool {
    let mut index = 0;
    while let Some(token) = args.get(index) {
        if token == "-C" {
            index += 2;
            continue;
        }
        if token.starts_with('-') {
            index += 1;
            continue;
        }
        return token == "test";
    }
    false
}

fn python_pytest(args: &[String]) -> bool {
    let mut index = 0;
    while let Some(token) = args.get(index) {
        if token == "-m" {
            return args
                .get(index + 1)
                .is_some_and(|module| module == "pytest" || module == "py.test");
        }
        if token == "-c" {
            index += 2;
        } else {
            index += 1;
        }
    }
    false
}

fn first_non_option(args: &[String]) -> Option<&str> {
    args.iter()
        .find(|token| !token.starts_with('-'))
        .map(String::as_str)
}

fn skip_wrappers(tokens: &[String]) -> usize {
    let mut index = 0;
    loop {
        let Some(token) = tokens.get(index) else {
            return index;
        };
        if is_env_assignment(token) {
            index += 1;
            continue;
        }
        match command_name(token).as_str() {
            "env" => {
                index += 1;
                while tokens
                    .get(index)
                    .is_some_and(|arg| is_env_assignment(arg) || arg.starts_with('-'))
                {
                    index += 1;
                }
            }
            "command" | "exec" | "nohup" | "time" => {
                index += 1;
                while tokens.get(index).is_some_and(|arg| arg.starts_with('-')) {
                    index += 1;
                }
            }
            _ => return index,
        }
    }
}

fn is_env_assignment(token: &str) -> bool {
    let Some((name, _)) = token.split_once('=') else {
        return false;
    };
    let mut bytes = name.bytes();
    let Some(first) = bytes.next() else {
        return false;
    };
    (first == b'_' || first.is_ascii_alphabetic())
        && bytes.all(|byte| byte == b'_' || byte.is_ascii_alphanumeric())
}

fn command_name(token: &str) -> String {
    let base = token
        .rsplit(['/', '\\'])
        .next()
        .unwrap_or(token)
        .trim_matches(['(', '{', '!', ')', '}'])
        .to_ascii_lowercase();
    for suffix in [".exe", ".cmd", ".bat", ".com"] {
        if let Some(stem) = base.strip_suffix(suffix) {
            return stem.to_owned();
        }
    }
    base
}

/// Splits a command line on hard separators while respecting quotes.
fn split_segments(input: &str) -> Vec<&str> {
    let mut segments = Vec::new();
    let mut start = 0;
    let mut quote: Option<char> = None;
    let mut escaped = false;
    let mut chars = input.char_indices().peekable();
    while let Some((index, character)) = chars.next() {
        if escaped {
            escaped = false;
            continue;
        }
        if let Some(active) = quote {
            if character == '\\' && active == '"' {
                escaped = true;
            } else if character == active {
                quote = None;
            }
            continue;
        }
        match character {
            '\\' => escaped = true,
            '\'' | '"' => quote = Some(character),
            '\n' | ';' => {
                segments.push(&input[start..index]);
                start = index + character.len_utf8();
            }
            '&' | '|' => {
                let mut end = index + character.len_utf8();
                if chars.peek().is_some_and(|(_, next)| *next == character)
                    && let Some((next_index, next)) = chars.next()
                {
                    end = next_index + next.len_utf8();
                }
                segments.push(&input[start..index]);
                start = end;
            }
            _ => {}
        }
    }
    segments.push(&input[start..]);
    segments
}

fn canonical_or_self(path: &Path) -> PathBuf {
    std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf())
}

fn cargo_workspace(cwd: &Path) -> PathBuf {
    let start = canonical_or_self(cwd);
    let mut package_root = None;
    for directory in start.ancestors() {
        let manifest = directory.join("Cargo.toml");
        if manifest.is_file() {
            if package_root.is_none() {
                package_root = Some(directory.to_path_buf());
            }
            if manifest_declares_workspace(&manifest) {
                return directory.to_path_buf();
            }
        }
    }
    package_root.unwrap_or(start)
}

fn node_workspace(cwd: &Path) -> PathBuf {
    let start = canonical_or_self(cwd);
    for directory in start.ancestors() {
        if directory.join("package.json").is_file() {
            return directory.to_path_buf();
        }
    }
    start
}

fn manifest_declares_workspace(manifest: &Path) -> bool {
    std::fs::read_to_string(manifest).is_ok_and(|text| {
        text.lines().any(|line| {
            let line = line.trim();
            line.starts_with("[workspace]") || line.starts_with("[workspace.")
        })
    })
}
