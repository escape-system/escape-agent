//! Opportunistic ripwire delegation: binary discovery, invocation flags, and a hard-timeout child process runner.

use std::ffi::OsStr;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;

use tokio::process::Command;

/// One-line installation command for `redhat-et/ripwire` (zero-dep C++23 graph context engine).
pub const RIPWIRE_INSTALL_CMD: &str =
    "curl -fsSL https://raw.githubusercontent.com/redhat-et/ripwire/main/install.sh | sh";

/// Environment override for the hard ripwire timeout, in milliseconds.
pub const RIPWIRE_TIMEOUT_ENV: &str = "OPENCODE_RIPWIRE_TIMEOUT_MS";
/// Default hard timeout for a single ripwire invocation.
pub const DEFAULT_RIPWIRE_TIMEOUT_MS: u64 = 30_000;
/// Upper bound on the stderr copy surfaced in result metadata.
const RIPWIRE_STDERR_LIMIT_BYTES: usize = 2 * 1024;

/// Arguments passed to ripwire, plus the parsed form of the caller's request.
///
/// The flag set is **mode dependent** because ripwire reads different budgets
/// per verb. Measured on ripwire 0.6.8 over `crates/oc-tools/src`:
///
/// - `--for=TASK --max-tokens=300` exits 0 but emits the *full* bundle
///   (`--max-tokens is not read by --for`, 6825 B for 300/1200/absent alike).
/// - `--for=TASK --top-k=5` also exits 0 with the same 6825 B and a warning
///   (`--top-k is not read by --for`).
/// - `--for=TASK --token-budget=300` is honoured: 2047 B against 6825 B
///   unbudgeted, with no warning, so `--token-budget` is the only budget flag
///   the task lens reads.
/// - Without `--for`, `--max-tokens=300` gives 1357 B and `--max-tokens=3000`
///   gives 5274 B, while a bare `--token-budget=300` exits 3 (hard gate
///   exceeded), so the default map reads `--max-tokens`, not `--token-budget`.
/// - Without `--for`, `--top-k=5` gives 2022 B of ranked rows and
///   `--top-k=100` gives 8879 B, so the default map does read `--top-k`.
/// - With `--for`, `--limit=N` is accepted but switches the document schema
///   from the `<ctx>` bundle to `ripwire.candidate-page/v1`, and it is refused
///   together with `--detail`. The task lens therefore gets no row window: the
///   budget is the only bound, and `limit` caps the default/native maps only.
///
/// Passing the unread flag is not harmless: it changes nothing but writes a
/// warning to stderr that a status-only check never sees.
#[derive(Debug, Clone, PartialEq, Eq)]
struct RipwireInvocation {
    args: Vec<String>,
}

impl RipwireInvocation {
    fn new(target: &Path, query: &str, max_tokens: usize, limit: usize) -> Self {
        let mut args = vec![target.to_string_lossy().into_owned()];
        if query.is_empty() {
            // Default ranked map: this verb has no task lens, so the token
            // budget and the row window are both honoured.
            args.push(format!("--max-tokens={max_tokens}"));
            args.push(format!("--top-k={limit}"));
        } else {
            // Task lens: --token-budget is the only budget flag it reads.
            // --max-tokens and --top-k are accepted but ignored with a warning.
            args.push(format!("--for={query}"));
            args.push(format!("--token-budget={max_tokens}"));
        }
        Self { args }
    }
}

#[derive(Debug)]
pub(super) struct RipwireOutput {
    pub(super) stdout: String,
    pub(super) stderr: Vec<u8>,
}

#[derive(Debug)]
pub(super) enum RipwireFailure {
    /// The child was still running when the deadline elapsed; it was killed.
    Timeout { timeout: Duration },
    /// The child could not be started at all.
    Spawn { message: String },
    /// The child ran and exited non-zero.
    Exit { code: Option<i32>, stderr: Vec<u8> },
}

/// Hard timeout for one ripwire run, overridable with
/// [`RIPWIRE_TIMEOUT_ENV`]. A non-numeric or zero value falls back to the
/// default rather than disabling the guard.
fn ripwire_timeout_ms(raw: Option<&OsStr>) -> u64 {
    raw.and_then(OsStr::to_str)
        .and_then(|raw| raw.trim().parse::<u64>().ok())
        .filter(|ms| *ms > 0)
        .unwrap_or(DEFAULT_RIPWIRE_TIMEOUT_MS)
}

pub(crate) fn configured_ripwire_timeout() -> Duration {
    let raw = std::env::var_os(RIPWIRE_TIMEOUT_ENV);
    Duration::from_millis(ripwire_timeout_ms(raw.as_deref()))
}

/// Strip ripwire's inline glossary comments (`<!-- ripwire ... -->` and
/// `<!-- t=... -->`) while leaving telemetry comments and non-ripwire output
/// untouched.
pub(super) fn compact_ripwire_cli_output(raw: &str) -> String {
    if !raw.contains("<!-- ripwire ") && !raw.contains("<!-- t=") {
        return raw.to_string();
    }
    let mut out = String::with_capacity(raw.len());
    let mut rest = raw;
    while let Some(start) = rest.find("<!--") {
        let (before, after_open) = rest.split_at(start);
        out.push_str(before);
        let Some(end_rel) = after_open.find("-->") else {
            out.push_str(after_open);
            rest = "";
            break;
        };
        let comment = &after_open[..end_rel + 3];
        let is_glossary = comment.starts_with("<!-- ripwire ") || comment.starts_with("<!-- t=");
        if !is_glossary {
            out.push_str(comment);
        }
        rest = &after_open[end_rel + 3..];
    }
    out.push_str(rest);
    collapse_blank_lines(&out).trim().to_string()
}

/// Filter and budget-trim `ripwire --mcp` `analyze` / `rank_by` `<r>...</r>` XML
/// so empty-query `map` fills `max_tokens` with symbols instead of glossary text.
pub(super) fn trim_analyze_xml_to_budget(
    xml: &str,
    rel_prefix: Option<&str>,
    max_tokens: usize,
    limit: usize,
) -> String {
    let cleaned = compact_ripwire_cli_output(xml);
    let max_chars = max_tokens.saturating_mul(4).max(200);
    let Some(r_start) = cleaned.find("<r>") else {
        return truncate_to_char_budget(&cleaned, max_chars);
    };

    let normalized_prefix = rel_prefix
        .map(|p| p.replace('\\', "/").trim_matches('/').to_string())
        .filter(|p| !p.is_empty() && p != ".");

    let mut stats_comment = None;
    if let Some(files_idx) = cleaned.find("<!-- files=")
        && let Some(end_rel) = cleaned[files_idx..].find("-->")
    {
        stats_comment = Some(&cleaned[files_idx..files_idx + end_rel + 3]);
    }
    let footer_reserve = stats_comment.map_or(0, |s| s.len() + 1) + 16;

    let mut out = String::from("<r>\n");
    let mut cursor = r_start + 3;
    let mut kept_files = 0usize;
    let mut kept_symbols = 0usize;

    while let Some(rel_f) = cleaned[cursor..].find("<f ") {
        let f_start = cursor + rel_f;
        let Some(f_end_rel) = cleaned[f_start..].find("</f>") else {
            break;
        };
        let block = &cleaned[f_start..f_start + f_end_rel + 4];
        cursor = f_start + f_end_rel + 4;

        let file_path = extract_xml_attr(block, "p").unwrap_or_default();
        if let Some(prefix) = &normalized_prefix {
            let file_norm = file_path.replace('\\', "/");
            let matches = file_norm == *prefix || file_norm.starts_with(&format!("{prefix}/"));
            if !matches {
                continue;
            }
        }

        let block_syms = block.matches("<s ").count().max(1);
        if kept_symbols > 0 && kept_symbols + block_syms > limit {
            break;
        }
        if out.len() + block.len() + footer_reserve > max_chars && kept_files > 0 {
            break;
        }

        out.push_str("  ");
        out.push_str(block);
        out.push('\n');
        kept_files += 1;
        kept_symbols += block_syms;
    }

    if let Some(stats) = stats_comment {
        out.push_str(stats);
        out.push('\n');
    }
    out.push_str("</r>");
    out
}

/// Remove comment-only lines and collapse blank line runs for `expand` with
/// `compress: true`. Preserves Rust attributes (`#[...]` / `#![...]`).
pub(super) fn strip_comments_and_blank_runs(body: &str) -> String {
    let mut out = String::with_capacity(body.len());
    let mut in_block_comment = false;
    let mut prev_blank = false;

    for line in body.lines() {
        let trimmed = line.trim();
        if in_block_comment {
            if let Some(end_idx) = trimmed.find("*/") {
                in_block_comment = false;
                let after = trimmed[end_idx + 2..].trim();
                if !after.is_empty() {
                    out.push_str(after);
                    out.push('\n');
                    prev_blank = false;
                }
            }
            continue;
        }

        if trimmed.is_empty() {
            if !prev_blank && !out.is_empty() {
                out.push('\n');
                prev_blank = true;
            }
            continue;
        }

        if trimmed.starts_with("/*") {
            if !trimmed.contains("*/") {
                in_block_comment = true;
            }
            continue;
        }

        let is_hash_comment = trimmed.starts_with('#')
            && !trimmed.starts_with("#[")
            && !trimmed.starts_with("#![")
            && !trimmed.starts_with("#include")
            && !trimmed.starts_with("#define")
            && !trimmed.starts_with("#if")
            && !trimmed.starts_with("#else")
            && !trimmed.starts_with("#endif");
        if trimmed.starts_with("//") || trimmed.starts_with("-- ") || is_hash_comment {
            continue;
        }

        out.push_str(line);
        out.push('\n');
        prev_blank = false;
    }

    out.trim_end().to_string()
}

fn extract_xml_attr<'a>(tag: &'a str, attr: &str) -> Option<&'a str> {
    let needle = format!("{attr}=\"");
    let start = tag.find(&needle)? + needle.len();
    let end = tag[start..].find('"')?;
    Some(&tag[start..start + end])
}

fn collapse_blank_lines(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut prev_blank = false;
    for line in text.lines() {
        if line.trim().is_empty() {
            if !prev_blank && !out.is_empty() {
                out.push('\n');
                prev_blank = true;
            }
        } else {
            out.push_str(line);
            out.push('\n');
            prev_blank = false;
        }
    }
    out
}

fn truncate_to_char_budget(text: &str, max_chars: usize) -> String {
    if text.len() <= max_chars {
        return text.to_string();
    }
    let mut end = max_chars;
    while end > 0 && !text.is_char_boundary(end) {
        end -= 1;
    }
    if let Some(nl) = text[..end].rfind('\n')
        && nl > max_chars / 2
    {
        end = nl;
    }
    format!("{}\n<!-- truncated to {max_chars} chars -->", &text[..end])
}

/// Trim captured stderr to at most [`RIPWIRE_STDERR_LIMIT_BYTES`] and return
/// `None` when it carries no text.
pub(super) fn trimmed_stderr(stderr: &[u8]) -> Option<String> {
    let text = String::from_utf8_lossy(stderr);
    let text = text.trim();
    if text.is_empty() {
        return None;
    }
    if text.len() <= RIPWIRE_STDERR_LIMIT_BYTES {
        return Some(text.to_string());
    }
    // Trim on a char boundary so lossy multi-byte input cannot panic a slice.
    let mut end = RIPWIRE_STDERR_LIMIT_BYTES;
    while end > 0 && !text.is_char_boundary(end) {
        end -= 1;
    }
    Some(format!("{}…", &text[..end]))
}

/// Run ripwire with a hard deadline.
///
/// Both pipes are drained on dedicated blocking tasks so a child that fills a
/// pipe buffer cannot deadlock the wait loop, and `kill_on_drop` guarantees the
/// process dies when the future is dropped (caller cancelled, timeout fired).
pub(super) async fn run_ripwire(
    binary: &Path,
    target: &Path,
    query: &str,
    max_tokens: usize,
    limit: usize,
) -> Result<RipwireOutput, RipwireFailure> {
    let invocation = RipwireInvocation::new(target, query, max_tokens, limit);
    run_ripwire_args(binary, &invocation.args).await
}

/// Run `ripwire` with explicit CLI arguments under [`configured_ripwire_timeout`].
pub(super) async fn run_ripwire_args<S: AsRef<OsStr>>(
    binary: &Path,
    args: &[S],
) -> Result<RipwireOutput, RipwireFailure> {
    let timeout = configured_ripwire_timeout();

    let mut child = Command::new(binary)
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .map_err(|error| RipwireFailure::Spawn {
            message: format!("failed to start {}: {error}", binary.display()),
        })?;

    let stdout = child.stdout.take().ok_or_else(|| RipwireFailure::Spawn {
        message: "ripwire stdout pipe unavailable".to_string(),
    })?;
    let stderr = child.stderr.take().ok_or_else(|| RipwireFailure::Spawn {
        message: "ripwire stderr pipe unavailable".to_string(),
    })?;
    let stdout_task = tokio::spawn(read_to_end(stdout));
    let stderr_task = tokio::spawn(read_to_end(stderr));

    let status = match tokio::time::timeout(timeout, child.wait()).await {
        Ok(Ok(status)) => status,
        Ok(Err(error)) => {
            let _ = child.kill().await;
            stdout_task.abort();
            stderr_task.abort();
            return Err(RipwireFailure::Spawn {
                message: format!("failed to wait for ripwire: {error}"),
            });
        }
        Err(_) => {
            let _ = child.kill().await;
            // kill() reaps, so wait() returns immediately; this also lets the
            // pipe readers observe EOF instead of blocking forever.
            let _ = child.wait().await;
            stdout_task.abort();
            stderr_task.abort();
            return Err(RipwireFailure::Timeout { timeout });
        }
    };

    let stdout = join_reader(stdout_task).await;
    let stderr = join_reader(stderr_task).await;
    if status.success() {
        Ok(RipwireOutput {
            stdout: String::from_utf8_lossy(&stdout).into_owned(),
            stderr,
        })
    } else {
        Err(RipwireFailure::Exit {
            code: status.code(),
            stderr,
        })
    }
}

async fn read_to_end(mut reader: impl tokio::io::AsyncRead + Unpin) -> Vec<u8> {
    let mut buffer = Vec::new();
    let _ = tokio::io::AsyncReadExt::read_to_end(&mut reader, &mut buffer).await;
    buffer
}

async fn join_reader(task: tokio::task::JoinHandle<Vec<u8>>) -> Vec<u8> {
    match task.await {
        Ok(bytes) => bytes,
        // A killed child closes its pipes; an aborted reader contributes
        // nothing rather than failing the whole call.
        Err(error) if error.is_cancelled() => Vec::new(),
        Err(_) => Vec::new(),
    }
}

/// Resolve candidates for [`find_ripwire_binary`] from explicit env values so
/// discovery is unit-testable without mutating the process environment.
///
/// Order: explicit `RIPWIRE_PATH`, `$HOME/.local/bin/ripwire`, then every
/// `PATH` entry. There is no hardcoded home directory: a fixed `/root/...`
/// candidate wrong on every non-root install and on root installs with a
/// different `$HOME`.
fn ripwire_candidates(ripwire_path: Option<&OsStr>, home: Option<&OsStr>) -> Vec<PathBuf> {
    let mut candidates = Vec::new();
    if let Some(custom) = ripwire_path {
        candidates.push(PathBuf::from(custom));
    }
    if let Some(home) = home {
        candidates.push(PathBuf::from(home).join(".local/bin/ripwire"));
    }
    candidates
}

/// Whether `path` names a regular file the current user may execute.
///
/// Discovery must not accept a readable-but-not-executable `ripwire`: spawning
/// it would fail with `EACCES` and turn every `map` into a spawn error.
#[must_use]
pub fn is_executable_file(path: &Path) -> bool {
    let Ok(metadata) = fs::metadata(path) else {
        return false;
    };
    if !metadata.is_file() {
        return false;
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        metadata.permissions().mode() & 0o111 != 0
    }
    #[cfg(not(unix))]
    {
        true
    }
}

/// Pure discovery core: every environment input is a parameter so the search
/// order and the executability rule are testable without touching the process
/// environment (which edition 2024 only allows through `unsafe`).
fn resolve_ripwire_binary(
    ripwire_path: Option<&OsStr>,
    home: Option<&OsStr>,
    path_var: Option<&OsStr>,
) -> Option<PathBuf> {
    for candidate in ripwire_candidates(ripwire_path, home) {
        if is_executable_file(&candidate) {
            return Some(candidate);
        }
    }
    for dir in std::env::split_paths(path_var?) {
        let candidate = dir.join("ripwire");
        if is_executable_file(&candidate) {
            return Some(candidate);
        }
    }
    None
}

pub(crate) fn find_ripwire_binary() -> Option<PathBuf> {
    let ripwire_path = std::env::var_os("RIPWIRE_PATH");
    let home = std::env::var_os("HOME");
    let path_var = std::env::var_os("PATH");
    resolve_ripwire_binary(
        ripwire_path.as_deref(),
        home.as_deref(),
        path_var.as_deref(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    #[cfg(unix)]
    use std::os::unix::fs::PermissionsExt as _;

    #[cfg(unix)]
    fn executable_script(dir: &Path, name: &str) -> PathBuf {
        let path = dir.join(name);
        fs::write(&path, "#!/bin/sh\nexit 0\n").unwrap();
        let mut perms = fs::metadata(&path).unwrap().permissions();
        perms.set_mode(0o755);
        fs::set_permissions(&path, perms).unwrap();
        path
    }

    #[cfg(unix)]
    #[test]
    fn discovery_requires_the_executable_bit() {
        let dir = tempfile::TempDir::new().unwrap();
        let plain = dir.path().join("ripwire");
        fs::write(&plain, "#!/bin/sh\nexit 0\n").unwrap();
        let mut perms = fs::metadata(&plain).unwrap().permissions();
        perms.set_mode(0o644);
        fs::set_permissions(&plain, perms.clone()).unwrap();

        assert!(!is_executable_file(&plain), "mode 0644 is not executable");
        assert!(
            resolve_ripwire_binary(Some(plain.as_os_str()), None, None).is_none(),
            "a readable but non-executable RIPWIRE_PATH must be rejected"
        );

        perms.set_mode(0o755);
        fs::set_permissions(&plain, perms).unwrap();
        assert!(is_executable_file(&plain));
        assert_eq!(
            resolve_ripwire_binary(Some(plain.as_os_str()), None, None),
            Some(plain.clone())
        );
        assert!(
            resolve_ripwire_binary(Some(dir.path().as_os_str()), None, None).is_none(),
            "a directory is not an executable file"
        );
    }

    #[cfg(unix)]
    #[test]
    fn discovery_order_and_no_hardcoded_home() {
        let root = tempfile::TempDir::new().unwrap();
        let custom = executable_script(root.path(), "custom-ripwire");
        let home = root.path().join("home");
        let home_bin = home.join(".local/bin");
        fs::create_dir_all(&home_bin).unwrap();
        let home_ripwire = executable_script(&home_bin, "ripwire");
        let on_path = root.path().join("pathdir");
        fs::create_dir_all(&on_path).unwrap();
        let path_ripwire = executable_script(&on_path, "ripwire");

        // RIPWIRE_PATH wins over $HOME and PATH.
        assert_eq!(
            resolve_ripwire_binary(
                Some(custom.as_os_str()),
                Some(home.as_os_str()),
                Some(on_path.as_os_str())
            ),
            Some(custom.clone())
        );
        // $HOME/.local/bin is second.
        assert_eq!(
            resolve_ripwire_binary(None, Some(home.as_os_str()), Some(on_path.as_os_str())),
            Some(home_ripwire.clone())
        );
        // PATH is the last resort.
        assert_eq!(
            resolve_ripwire_binary(None, None, Some(on_path.as_os_str())),
            Some(path_ripwire.clone())
        );
        // A missing HOME is skipped, not replaced by a hardcoded one.
        assert_eq!(
            resolve_ripwire_binary(None, None, None),
            None,
            "no inputs must not resolve a hardcoded /root path"
        );

        // The old hardcoded candidate must not be reachable through HOME when
        // HOME points elsewhere: only the given home is searched.
        let other_home = root.path().join("other-home");
        fs::create_dir_all(&other_home).unwrap();
        assert_eq!(
            resolve_ripwire_binary(
                None,
                Some(other_home.as_os_str()),
                Some(on_path.as_os_str())
            ),
            Some(path_ripwire),
            "an unrelated HOME falls through to PATH"
        );
        // Unset HOME and PATH find nothing even though /root/.local/bin/ripwire
        // exists on some machines: that path is no longer a candidate.
        assert_eq!(
            resolve_ripwire_binary(None, None, Some(OsStr::new(""))),
            None
        );
    }

    #[test]
    fn invocation_flags_follow_the_measured_mode_contract() {
        let target = Path::new("/tmp/project");

        let default_map = RipwireInvocation::new(target, "", 1200, 50);
        assert_eq!(
            default_map.args,
            vec![
                "/tmp/project".to_string(),
                "--max-tokens=1200".to_string(),
                "--top-k=50".to_string()
            ]
        );

        let task_lens = RipwireInvocation::new(target, "server start", 1200, 50);
        assert_eq!(
            task_lens.args,
            vec![
                "/tmp/project".to_string(),
                "--for=server start".to_string(),
                "--token-budget=1200".to_string()
            ]
        );
        // ripwire 0.6.8 ignores both flags under --for and warns on stderr.
        assert!(
            !task_lens.args.iter().any(|arg| arg.starts_with("--top-k")),
            "--top-k is not read by --for"
        );
        assert!(
            !task_lens
                .args
                .iter()
                .any(|arg| arg.starts_with("--max-tokens")),
            "--max-tokens is not read by --for"
        );
    }

    #[test]
    fn timeout_override_rejects_zero_and_garbage() {
        assert_eq!(
            ripwire_timeout_ms(None),
            DEFAULT_RIPWIRE_TIMEOUT_MS,
            "unset env uses the default"
        );
        assert_eq!(
            ripwire_timeout_ms(Some(OsStr::new("250"))),
            250,
            "a numeric override is honoured"
        );
        assert_eq!(
            ripwire_timeout_ms(Some(OsStr::new("0"))),
            DEFAULT_RIPWIRE_TIMEOUT_MS,
            "zero must not disable the guard"
        );
        assert_eq!(
            ripwire_timeout_ms(Some(OsStr::new("soon"))),
            DEFAULT_RIPWIRE_TIMEOUT_MS,
            "garbage falls back instead of failing"
        );
    }

    #[test]
    fn stderr_is_trimmed_and_bounded() {
        assert_eq!(trimmed_stderr(b""), None);
        assert_eq!(trimmed_stderr(b"   \n\t"), None, "whitespace is not text");
        assert_eq!(
            trimmed_stderr(b" warning: nope \n"),
            Some("warning: nope".to_string())
        );

        let long = format!("{}END", "x".repeat(RIPWIRE_STDERR_LIMIT_BYTES));
        let trimmed = trimmed_stderr(long.as_bytes()).unwrap();
        assert!(
            trimmed.len() <= RIPWIRE_STDERR_LIMIT_BYTES + '…'.len_utf8(),
            "trimmed stderr stays bounded, got {} bytes",
            trimmed.len()
        );
        assert!(trimmed.ends_with('…'), "a cut is marked: {trimmed}");
        assert!(!trimmed.contains("END"), "the tail is dropped");

        // Multi-byte input must not panic on a non-char-boundary cut.
        let unicode = "é".repeat(RIPWIRE_STDERR_LIMIT_BYTES);
        assert!(trimmed_stderr(unicode.as_bytes()).is_some());
    }

    #[test]
    fn compact_cli_strips_only_glossary_comments() {
        let sample = "<!-- ripwire communities schema=v1 glossary -->\n\
                      <communities>\n\
                      <!-- t=modscope glossary -->\n\
                      <community id=\"c1\"/>\n\
                      <!-- files=10 symbols=42 -->\n\
                      </communities>";
        let compacted = compact_ripwire_cli_output(sample);
        assert!(!compacted.contains("ripwire communities"));
        assert!(!compacted.contains("t=modscope"));
        assert!(compacted.contains("<community id=\"c1\"/>"));
        assert!(compacted.contains("<!-- files=10 symbols=42 -->"));

        let mock_echo = "/tmp/repo\n--max-tokens=1200\n--top-k=7\n";
        assert_eq!(compact_ripwire_cli_output(mock_echo), mock_echo);
    }

    #[test]
    fn trim_analyze_xml_filters_prefix_and_fills_budget() {
        let xml = "<r>\
            <f p=\"src/a.rs\"><s t=\"fn\" n=\"alpha\"></s><s t=\"fn\" n=\"beta\"></s></f>\
            <f p=\"src/b.rs\"><s t=\"fn\" n=\"gamma\"></s><s t=\"fn\" n=\"delta\"></s></f>\
            <f p=\"other/c.rs\"><s t=\"fn\" n=\"zeta\"></s></f>\
            <!-- files=3 symbols=5 -->\
            <about schema=\"ripwire.map/v1\"/></r>";

        let scoped = trim_analyze_xml_to_budget(xml, Some("src"), 400, 10);
        assert!(scoped.contains("src/a.rs"));
        assert!(scoped.contains("src/b.rs"));
        assert!(!scoped.contains("other/c.rs"));
        assert!(scoped.contains("alpha"));
        assert!(scoped.contains("<!-- files=3 symbols=5 -->"));

        let capped_syms = trim_analyze_xml_to_budget(xml, None, 1200, 2);
        assert!(capped_syms.contains("src/a.rs"));
        assert!(!capped_syms.contains("src/b.rs"));
    }

    #[test]
    fn strip_comments_preserves_rust_attributes_and_code() {
        let body = "#[derive(Debug)]\n// doc comment\npub fn run() {\n    /* block\n       comment */\n\n\n    let x = 1;\n}\n";
        let compressed = strip_comments_and_blank_runs(body);
        assert!(compressed.contains("#[derive(Debug)]"));
        assert!(!compressed.contains("doc comment"));
        assert!(!compressed.contains("block"));
        assert!(compressed.contains("pub fn run() {"));
        assert!(compressed.contains("let x = 1;"));
    }
}
