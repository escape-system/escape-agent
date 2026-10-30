//! Textual `{env:VAR}` and `{file:path}` substitution applied before parsing.

use std::path::{Path, PathBuf};

/// A `{file:...}` reference that could not be read. The token expands to an
/// empty string; the caller records the warning.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct MissingFile {
    pub reference: String,
    pub path: PathBuf,
}

/// Apply substitutions to `text`.
///
/// `dir` is the directory `{file:...}` references resolve against (the
/// document's directory, or the location directory for inline content), and
/// `home` is expanded for `~/` references. `lookup` resolves environment
/// variables; the caller layers `LoadOptions::env` over the process env.
pub(crate) fn substitute(
    text: &str,
    dir: &Path,
    home: &Path,
    lookup: &dyn Fn(&str) -> Option<String>,
) -> (String, Vec<MissingFile>) {
    let (first, missing) = run(text, dir, home, lookup, false);
    // Escape only when raw values break the JSON, so configs that parse today
    // keep their meaning (upstream behavior).
    if text.contains("{env:") && crate::parse_jsonc(&first).is_err() {
        let (escaped, missing) = run(text, dir, home, lookup, true);
        return (escaped, missing);
    }
    (first, missing)
}

fn run(
    text: &str,
    dir: &Path,
    home: &Path,
    lookup: &dyn Fn(&str) -> Option<String>,
    escape_env: bool,
) -> (String, Vec<MissingFile>) {
    let with_env = replace_env(text, lookup, escape_env);
    replace_files(&with_env, dir, home)
}

fn replace_env(text: &str, lookup: &dyn Fn(&str) -> Option<String>, escape: bool) -> String {
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(index) = rest.find("{env:") {
        let after = &rest[index + "{env:".len()..];
        let Some(end) = after.find('}') else {
            break;
        };
        out.push_str(&rest[..index]);
        let name = &after[..end];
        let value = lookup(name).unwrap_or_default();
        if escape {
            out.push_str(&json_escape(&value));
        } else {
            out.push_str(&value);
        }
        rest = &after[end + 1..];
    }
    out.push_str(rest);
    out
}

fn replace_files(text: &str, dir: &Path, home: &Path) -> (String, Vec<MissingFile>) {
    let mut out = String::with_capacity(text.len());
    let mut missing = Vec::new();
    let mut cursor = 0;
    while let Some(index) = text[cursor..].find("{file:") {
        let start = cursor + index;
        let after = &text[start + "{file:".len()..];
        let Some(end) = after.find('}') else {
            break;
        };
        let token = &text[start..=start + "{file:".len() + end];

        // `//` comment lines keep the token verbatim, like upstream.
        let line_start = text[..start].rfind('\n').map_or(0, |position| position + 1);
        if text[line_start..start].trim_start().starts_with("//") {
            out.push_str(&text[cursor..start + token.len()]);
            cursor = start + token.len();
            continue;
        }

        let reference = &after[..end];
        let expanded = reference
            .strip_prefix("~/")
            .map_or_else(|| PathBuf::from(reference), |relative| home.join(relative));
        let path = if expanded.is_absolute() {
            expanded
        } else {
            dir.join(expanded)
        };
        let content = if let Ok(content) = std::fs::read_to_string(&path) {
            content.trim().to_owned()
        } else {
            missing.push(MissingFile {
                reference: reference.to_owned(),
                path: path.clone(),
            });
            String::new()
        };
        out.push_str(&text[cursor..start]);
        out.push_str(&json_escape(&content));
        cursor = start + token.len();
    }
    out.push_str(&text[cursor..]);
    (out, missing)
}

/// JSON string escaping without the surrounding quotes.
fn json_escape(value: &str) -> String {
    let mut encoded = serde_json::to_string(value).unwrap_or_else(|_| "\"\"".to_owned());
    encoded.remove(0);
    encoded.pop();
    encoded
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use super::substitute;

    #[test]
    fn env_values_are_escaped_only_when_needed() {
        let mut env = HashMap::new();
        env.insert("WINPATH".to_owned(), "C:\\temp\\config".to_owned());
        let lookup = |name: &str| env.get(name).cloned();
        let (text, missing) = substitute(
            r#"{"shell": "{env:WINPATH}"}"#,
            std::path::Path::new("."),
            std::path::Path::new("/home/u"),
            &lookup,
        );
        assert!(missing.is_empty());
        let value: serde_json::Value = serde_json::from_str(&text).expect("valid json");
        assert_eq!(value["shell"], "C:\\temp\\config");
    }

    #[test]
    fn file_references_and_comments() {
        let directory = tempfile::tempdir().expect("temp dir");
        std::fs::write(directory.path().join("prompt.txt"), "  hello  \n").expect("write");
        let lookup = |_: &str| None;
        let (text, missing) = substitute(
            "{\n// {file:missing}\n\"system\": \"{file:prompt.txt}\",\n\"other\": \"{file:nope.txt}\"\n}",
            directory.path(),
            directory.path(),
            &lookup,
        );
        let value = crate::parse_jsonc(&text).expect("valid jsonc");
        assert_eq!(value["system"], "hello");
        assert_eq!(value["other"], "");
        assert_eq!(missing.len(), 1);
        assert_eq!(missing[0].reference, "nope.txt");
    }
}
