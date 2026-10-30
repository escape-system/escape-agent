//! Git process, error mapping and porcelain parsing for worktree routes.

use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::process::Stdio;

use oc_types::ApiError;
use tokio::io::AsyncReadExt;
use tokio::process::Command;

use crate::data::git::GitOutput;

use super::paths::git_path;
use super::{GIT_TIMEOUT, MAX_STDERR, MAX_STDOUT, worktree_error};

/// One `worktree <path>` record from `git worktree list --porcelain`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct ListedWorktree {
    pub(super) directory: PathBuf,
    pub(super) branch: Option<String>,
}

/// Run git in `directory` with a deadline and bounded output.
pub(super) async fn run_git(
    directory: &Path,
    arguments: &[OsString],
) -> Result<GitOutput, ApiError> {
    let directory = git_path(directory);
    let mut command = Command::new("git");
    command
        .arg("-C")
        .arg(&directory)
        .args(arguments)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    let mut child = command.spawn().map_err(|error| {
        if error.kind() == std::io::ErrorKind::NotFound {
            ApiError::service_unavailable("git executable not available")
        } else {
            tracing::error!(error = %error, "failed to spawn git");
            ApiError::internal("Internal error")
        }
    })?;
    let Some(mut stdout) = child.stdout.take() else {
        return Err(ApiError::internal("Internal error"));
    };
    let Some(mut stderr) = child.stderr.take() else {
        return Err(ApiError::internal("Internal error"));
    };

    let mut out = Vec::new();
    let mut err = Vec::new();
    let mut out_take = (&mut stdout).take(MAX_STDOUT as u64);
    let mut err_take = (&mut stderr).take(MAX_STDERR as u64);
    let read = async {
        tokio::join!(
            out_take.read_to_end(&mut out),
            err_take.read_to_end(&mut err)
        )
    };
    let (out_result, err_result) = tokio::time::timeout(GIT_TIMEOUT, read)
        .await
        .map_err(|_| ApiError::service_unavailable("git command timed out after 30 s"))?;
    let read_error = |error: std::io::Error| {
        tracing::error!(error = %error, "failed to read git output");
        ApiError::internal("Internal error")
    };
    out_result.map_err(read_error)?;
    err_result.map_err(read_error)?;

    let status = child.wait().await.map_err(|error| {
        tracing::error!(error = %error, "failed to wait for git");
        ApiError::internal("Internal error")
    })?;
    Ok(GitOutput {
        code: status.code(),
        stdout: out,
        stderr: String::from_utf8_lossy(&err).into_owned(),
        truncated: false,
    })
}

pub(super) fn git_worktree_error(
    operation: &str,
    output: &GitOutput,
    force_hint: bool,
) -> ApiError {
    let diagnostic = output.message();
    let message = if diagnostic.is_empty() {
        format!("git worktree {operation} failed")
    } else {
        format!("git worktree {operation} failed: {diagnostic}")
    };
    let force_required = force_hint && needs_force(&diagnostic);
    worktree_error(message, force_required.then_some(true))
}

fn needs_force(message: &str) -> bool {
    let lower = message.to_ascii_lowercase();
    lower.contains("contains modified or untracked files") || lower.contains("is dirty")
}

/// The repository root containing `directory`, or a `WorktreeError`.
pub(super) async fn repository_root(directory: &Path) -> Result<PathBuf, ApiError> {
    let arguments = [
        OsString::from("rev-parse"),
        OsString::from("--show-toplevel"),
    ];
    let output = run_git(directory, &arguments).await?;
    if !output.ok() {
        let diagnostic = output.message();
        let message = if diagnostic.is_empty() {
            format!("{} is not a git repository", directory.display())
        } else {
            format!(
                "{} is not a git repository: {diagnostic}",
                directory.display()
            )
        };
        return Err(worktree_error(message, None));
    }
    let root = output.text();
    let root = root.trim();
    if root.is_empty() {
        return Err(worktree_error(
            format!("{} is not a git repository", directory.display()),
            None,
        ));
    }
    Ok(PathBuf::from(root))
}

pub(super) async fn remove_checkout(
    repository: &Path,
    target: &Path,
    force: bool,
) -> Result<(), ApiError> {
    let mut arguments = vec![OsString::from("worktree"), OsString::from("remove")];
    if force {
        arguments.push(OsString::from("--force"));
    }
    arguments.push(OsString::from("--"));
    arguments.push(target.as_os_str().to_os_string());
    let output = run_git(repository, &arguments).await?;
    if output.ok() {
        Ok(())
    } else {
        Err(git_worktree_error("remove", &output, true))
    }
}

/// `git worktree prune --expire now`, ignoring failures during cleanup.
pub(super) async fn prune(repository: &Path) {
    let arguments = [
        OsString::from("worktree"),
        OsString::from("prune"),
        OsString::from("--expire"),
        OsString::from("now"),
    ];
    let _ = run_git(repository, &arguments).await;
}

pub(super) fn parse_porcelain(text: &str) -> Vec<ListedWorktree> {
    let mut items = Vec::new();
    let mut current: Option<ListedWorktree> = None;
    for line in text.lines() {
        if line.is_empty() {
            if let Some(item) = current.take() {
                items.push(item);
            }
            continue;
        }
        if let Some(rest) = line.strip_prefix("worktree ") {
            if let Some(item) = current.take() {
                items.push(item);
            }
            current = Some(ListedWorktree {
                directory: PathBuf::from(unquote_git_path(rest)),
                branch: None,
            });
        } else if let Some(rest) = line.strip_prefix("branch ")
            && let Some(item) = current.as_mut()
        {
            item.branch = Some(rest.strip_prefix("refs/heads/").unwrap_or(rest).to_owned());
        }
    }
    if let Some(item) = current.take() {
        items.push(item);
    }
    items
}

/// Decode git's C-style quoting of a path.
///
/// Octal escapes name bytes, not code points, so the result is assembled as
/// bytes and decoded as UTF-8 at the end.
fn unquote_git_path(raw: &str) -> String {
    if raw.len() < 2 || !raw.starts_with('"') || !raw.ends_with('"') {
        return raw.to_owned();
    }
    let inner = &raw[1..raw.len() - 1];
    let mut out: Vec<u8> = Vec::with_capacity(inner.len());
    let mut characters = inner.chars().peekable();
    while let Some(character) = characters.next() {
        if character != '\\' {
            push_utf8(&mut out, character);
            continue;
        }
        match characters.next() {
            Some('n') => out.push(b'\n'),
            Some('t') => out.push(b'\t'),
            Some('r') => out.push(b'\r'),
            Some('b') => out.push(0x08),
            Some('f') => out.push(0x0c),
            Some('v') => out.push(0x0b),
            Some('a') => out.push(0x07),
            Some('"') => out.push(b'"'),
            Some(first @ '0'..='7') => {
                let mut value = first as u32 - '0' as u32;
                let mut digits = 1;
                while digits < 3 {
                    let Some(next @ '0'..='7') = characters.peek().copied() else {
                        break;
                    };
                    value = value * 8 + (next as u32 - '0' as u32);
                    characters.next();
                    digits += 1;
                }
                out.push(u8::try_from(value).unwrap_or(b'?'));
            }
            Some('\\') | None => out.push(b'\\'),
            Some(other) => push_utf8(&mut out, other),
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn push_utf8(out: &mut Vec<u8>, character: char) {
    let mut buffer = [0_u8; 4];
    out.extend_from_slice(character.encode_utf8(&mut buffer).as_bytes());
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn porcelain_parsing_handles_blocks_and_quoting() {
        let text = concat!(
            "worktree /repo\n",
            "HEAD 0123456789abcdef\n",
            "branch refs/heads/main\n",
            "\n",
            "worktree /repo/linked tree\n",
            "HEAD abcdef0123456789\n",
            "detached\n",
            "\n",
            "worktree \"/repo/space \\\"quote\\\"\"\n",
            "HEAD fedcba9876543210\n",
            "branch refs/heads/topic/x\n",
            "\n",
        );
        let parsed = parse_porcelain(text);
        assert_eq!(
            parsed,
            vec![
                ListedWorktree {
                    directory: PathBuf::from("/repo"),
                    branch: Some("main".to_owned()),
                },
                ListedWorktree {
                    directory: PathBuf::from("/repo/linked tree"),
                    branch: None,
                },
                ListedWorktree {
                    directory: PathBuf::from("/repo/space \"quote\""),
                    branch: Some("topic/x".to_owned()),
                },
            ]
        );
    }

    #[test]
    fn porcelain_without_trailing_blank_line_is_kept() {
        let parsed = parse_porcelain("worktree /repo\nHEAD abc\nbranch refs/heads/main\n");
        assert_eq!(parsed.len(), 1);
        assert_eq!(parsed[0].directory, PathBuf::from("/repo"));
        assert_eq!(parsed[0].branch.as_deref(), Some("main"));
    }

    #[test]
    fn octal_escapes_decode() {
        assert_eq!(unquote_git_path("\"caf\\303\\251\""), "café");
    }
}
