//! `file:` URI conversion for LSP payloads.

use std::path::{Path, PathBuf};

use percent_encoding::{AsciiSet, CONTROLS, percent_decode_str, utf8_percent_encode};

/// Bytes escaped in the path component. Slash and drive-letter colon are kept
/// literal, matching what editors send.
const PATH_SET: &AsciiSet = &CONTROLS
    .add(b' ')
    .add(b'"')
    .add(b'<')
    .add(b'>')
    .add(b'`')
    .add(b'#')
    .add(b'?')
    .add(b'{')
    .add(b'}')
    .add(b'|')
    .add(b'^')
    .add(b'[')
    .add(b']')
    .add(b'%')
    .add(b'\\');

/// Convert an absolute path to a percent-encoded `file:` URI.
///
/// `<` and `>` are legal in the LSP `DocumentUri` but escaped for safety.
#[must_use]
pub fn path_to_uri(path: &Path) -> String {
    let text = path.to_string_lossy().replace('\\', "/");
    if cfg!(windows) {
        let text = text.strip_prefix("//?/").unwrap_or(&text);
        let text = text.strip_prefix("//./").unwrap_or(text);
        let with_slash = if text.starts_with('/') {
            text.to_owned()
        } else {
            format!("/{text}")
        };
        format!("file://{}", utf8_percent_encode(&with_slash, PATH_SET))
    } else {
        let with_slash = if text.starts_with('/') {
            text
        } else {
            format!("/{text}")
        };
        format!("file://{}", utf8_percent_encode(&with_slash, PATH_SET))
    }
}

/// Parse a `file:` URI back to a path. Returns `None` for other schemes.
#[must_use]
pub fn uri_to_path(uri: &str) -> Option<PathBuf> {
    let rest = uri.strip_prefix("file://")?;
    let rest = rest.strip_prefix("localhost").unwrap_or(rest);
    let decoded = percent_decode_str(rest).decode_utf8().ok()?;
    let decoded = decoded.as_ref();
    if cfg!(windows) {
        let without_slash = decoded.strip_prefix('/').unwrap_or(decoded);
        // `C:/x` (possibly `c%3A` decoded) becomes `C:\x`.
        let mut chars = without_slash.chars();
        let is_drive = matches!(chars.next(), Some(letter) if letter.is_ascii_alphabetic())
            && chars.next() == Some(':');
        let path = if is_drive {
            without_slash.to_owned()
        } else {
            decoded.to_owned()
        };
        Some(PathBuf::from(path.replace('/', "\\")))
    } else {
        Some(PathBuf::from(decoded))
    }
}

/// Whether the URI addresses a local file.
#[must_use]
pub fn is_file_uri(uri: &str) -> bool {
    uri.starts_with("file://")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn absolute_paths_round_trip() {
        let path = if cfg!(windows) {
            PathBuf::from("C:\\work dir\\caf\u{e9}.rs")
        } else {
            PathBuf::from("/work dir/caf\u{e9}.rs")
        };
        let uri = path_to_uri(&path);
        assert!(uri.starts_with("file:///"), "{uri}");
        assert!(uri.contains("%20"), "{uri}");
        assert!(!uri.contains(' '), "{uri}");
        assert_eq!(uri_to_path(&uri).as_deref(), Some(path.as_path()));
    }

    #[test]
    fn reserved_characters_are_escaped() {
        let path = if cfg!(windows) {
            PathBuf::from("C:\\a#b?c%d\\x.rs")
        } else {
            PathBuf::from("/a#b?c%d/x.rs")
        };
        let uri = path_to_uri(&path);
        assert!(uri.contains("%23"), "{uri}");
        assert!(uri.contains("%3F"), "{uri}");
        assert!(uri.contains("%25"), "{uri}");
        assert_eq!(uri_to_path(&uri).as_deref(), Some(path.as_path()));
    }

    #[test]
    fn rejects_non_file_uris() {
        assert!(uri_to_path("https://example.com/a.rs").is_none());
        assert!(!is_file_uri("https://example.com"));
    }

    #[test]
    fn empty_path_is_still_a_file_uri() {
        assert!(path_to_uri(Path::new("")).starts_with("file://"));
    }
}
