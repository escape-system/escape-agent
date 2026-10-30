//! Markdown frontmatter reader.
//!
//! Skill, command and agent files carry a small YAML frontmatter block. The
//! workspace has no YAML dependency and the accepted grammar is deliberately
//! narrow (docs/07): unquoted or quoted scalars, booleans, numbers, and one
//! level of nested mappings (`metadata:`). Flow collections, block scalars,
//! anchors and deeper nesting are reported as unparsable frontmatter instead
//! of being guessed.

use serde_json::{Map, Number, Value};

/// A file split into its frontmatter map and body.
#[derive(Debug, Clone)]
pub struct Parsed {
    pub meta: Map<String, Value>,
    pub body: String,
    /// Present when the block could not be read as the supported subset.
    pub error: Option<String>,
}

/// Strip a leading UTF-8 BOM; configuration files are commonly written on
/// Windows and must not lose their first key.
fn strip_bom(text: &str) -> &str {
    text.strip_prefix('\u{feff}').unwrap_or(text)
}

pub fn parse(text: &str) -> Parsed {
    let text = strip_bom(text);
    let mut lines = text.split('\n');
    let first = lines.next().unwrap_or_default().trim_end_matches('\r');
    if first.trim() != "---" {
        return Parsed {
            meta: Map::new(),
            body: text.to_owned(),
            error: None,
        };
    }

    let mut meta_lines: Vec<(usize, &str)> = Vec::new();
    let mut body_lines: Vec<&str> = Vec::new();
    let mut closed = false;
    for (index, line) in lines.enumerate() {
        let line = line.trim_end_matches('\r');
        if !closed && matches!(line.trim(), "---" | "...") {
            closed = true;
            continue;
        }
        if closed {
            body_lines.push(line);
        } else {
            meta_lines.push((index + 2, line));
        }
    }

    if !closed {
        return Parsed {
            meta: Map::new(),
            body: text.to_owned(),
            error: Some("frontmatter opened with `---` has no closing delimiter".to_owned()),
        };
    }

    let (meta, error) = parse_meta(&meta_lines);
    Parsed {
        meta,
        body: body_lines.join("\n"),
        error,
    }
}

fn parse_meta(lines: &[(usize, &str)]) -> (Map<String, Value>, Option<String>) {
    let mut meta = Map::new();
    let mut error: Option<String> = None;
    let mut parent: Option<(String, usize)> = None;
    for (number, line) in lines {
        if line.trim().is_empty() || line.trim_start().starts_with('#') {
            continue;
        }
        if line.starts_with('\t') || line.trim_start().starts_with('\t') {
            report(
                &mut error,
                format!("frontmatter line {number}: tabs are not valid indentation"),
            );
            continue;
        }
        let indent = line.len() - line.trim_start_matches(' ').len();
        let content = line.trim();
        let Some((key, value)) = split_key(content) else {
            report(
                &mut error,
                format!("frontmatter line {number}: expected `key: value`"),
            );
            continue;
        };
        if indent == 0 {
            parent = None;
            if value.is_empty() {
                meta.insert(key.clone(), Value::Object(Map::new()));
                parent = Some((key, 0));
                continue;
            }
            match parse_scalar(value) {
                Ok(value) => {
                    meta.insert(key, value);
                }
                Err(message) => report(
                    &mut error,
                    format!("frontmatter line {number}: `{key}`: {message}"),
                ),
            }
            continue;
        }

        let Some((parent_key, _)) = parent.clone() else {
            report(
                &mut error,
                format!("frontmatter line {number}: indented value without a parent key"),
            );
            continue;
        };
        if value.is_empty() {
            report(
                &mut error,
                format!(
                    "frontmatter line {number}: nested mappings deeper than one level are not supported"
                ),
            );
            continue;
        }
        let Some(Value::Object(children)) = meta.get_mut(&parent_key) else {
            report(
                &mut error,
                format!("frontmatter line {number}: `{parent_key}` is not a mapping"),
            );
            continue;
        };
        match parse_scalar(value) {
            Ok(value) => {
                children.insert(key, value);
            }
            Err(message) => report(
                &mut error,
                format!("frontmatter line {number}: `{parent_key}.{key}`: {message}"),
            ),
        }
    }
    (meta, error)
}

fn report(slot: &mut Option<String>, message: String) {
    if slot.is_none() {
        *slot = Some(message);
    }
}

/// Split at the first `:` that is not inside a quoted key.
fn split_key(content: &str) -> Option<(String, &str)> {
    let mut quote: Option<char> = None;
    for (index, character) in content.char_indices() {
        match character {
            '"' | '\'' => match quote {
                Some(active) if active == character => quote = None,
                None => quote = Some(character),
                Some(_) => {}
            },
            ':' if quote.is_none() => {
                let key = content[..index].trim();
                if key.is_empty() {
                    return None;
                }
                return Some((key.to_owned(), content[index + 1..].trim_start()));
            }
            _ => {}
        }
    }
    None
}

fn parse_scalar(value: &str) -> Result<Value, String> {
    let value = strip_comment(value).trim();
    if value.is_empty() {
        return Err("missing value".to_owned());
    }
    if let Some(rest) = value.strip_prefix('"') {
        return parse_double_quoted(rest).map(Value::String);
    }
    if let Some(rest) = value.strip_prefix('\'') {
        return parse_single_quoted(rest).map(Value::String);
    }
    if value.starts_with('[') || value.starts_with('{') {
        return Err("flow collections are not supported".to_owned());
    }
    if value.starts_with('|') || value.starts_with('>') {
        return Err("block scalars are not supported".to_owned());
    }
    if value.starts_with('&') || value.starts_with('*') || value.starts_with("!!") {
        return Err("anchors, aliases and tags are not supported".to_owned());
    }
    if let Some(value) = bool_word(value) {
        return Ok(Value::Bool(value));
    }
    if matches!(value, "null" | "Null" | "NULL" | "~") {
        return Ok(Value::Null);
    }
    if let Ok(integer) = value.parse::<i64>() {
        return Ok(Value::from(integer));
    }
    if value.contains('.')
        && let Ok(float) = value.parse::<f64>()
        && let Some(number) = Number::from_f64(float)
    {
        return Ok(Value::Number(number));
    }
    Ok(Value::String(value.to_owned()))
}

fn parse_double_quoted(rest: &str) -> Result<String, String> {
    let mut out = String::new();
    let mut characters = rest.chars();
    while let Some(character) = characters.next() {
        match character {
            '"' => {
                if characters.as_str().trim().is_empty() {
                    return Ok(out);
                }
                return Err("trailing characters after quoted string".to_owned());
            }
            '\\' => match characters.next() {
                Some('n') => out.push('\n'),
                Some('r') => out.push('\r'),
                Some('t') => out.push('\t'),
                Some('"') => out.push('"'),
                Some('\\') => out.push('\\'),
                Some('/') => out.push('/'),
                Some(other) => out.push(other),
                None => return Err("unterminated escape sequence".to_owned()),
            },
            other => out.push(other),
        }
    }
    Err("unterminated quoted string".to_owned())
}

fn parse_single_quoted(rest: &str) -> Result<String, String> {
    let mut out = String::new();
    let mut characters = rest.chars();
    while let Some(character) = characters.next() {
        match character {
            '\'' => {
                let mut probe = characters.clone();
                if probe.next() == Some('\'') {
                    characters.next();
                    out.push('\'');
                } else if characters.as_str().trim().is_empty() {
                    return Ok(out);
                } else {
                    return Err("trailing characters after quoted string".to_owned());
                }
            }
            other => out.push(other),
        }
    }
    Err("unterminated quoted string".to_owned())
}

/// Drop a trailing ` # comment` outside of quotes.
fn strip_comment(value: &str) -> &str {
    let mut quote: Option<char> = None;
    let mut previous = '\0';
    for (index, character) in value.char_indices() {
        match character {
            '"' | '\'' => match quote {
                Some(active) if active == character => quote = None,
                None => quote = Some(character),
                Some(_) => {}
            },
            '#' if quote.is_none() && (previous == ' ' || previous == '\t') => {
                return &value[..index];
            }
            _ => {}
        }
        previous = character;
    }
    value
}

fn bool_word(value: &str) -> Option<bool> {
    match value.to_ascii_lowercase().as_str() {
        "true" | "yes" | "on" => Some(true),
        "false" | "no" | "off" => Some(false),
        _ => None,
    }
}

fn coerce_bool_text(value: &str) -> Option<bool> {
    match value.to_ascii_lowercase().as_str() {
        "true" | "yes" | "on" | "1" => Some(true),
        "false" | "no" | "off" | "0" => Some(false),
        _ => None,
    }
}

/// Boolean coercion shared by skills, commands and agent frontmatter:
/// `true/yes/on/1` and `false/no/off/0`, plus JSON booleans and numbers.
pub(crate) fn coerce_bool(value: &Value) -> Option<bool> {
    match value {
        Value::Bool(value) => Some(*value),
        Value::Number(number) => match number.as_i64() {
            Some(1) => Some(true),
            Some(0) => Some(false),
            _ => None,
        },
        Value::String(text) => coerce_bool_text(text.trim()),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::{coerce_bool, parse};
    use serde_json::Value;

    #[test]
    fn splits_frontmatter_and_body() {
        let parsed =
            parse("---\nname: Build Test\ndescription: \"quoted: value\"\n---\nBody line\n");
        assert!(parsed.error.is_none());
        assert_eq!(parsed.meta["name"], "Build Test");
        assert_eq!(parsed.meta["description"], "quoted: value");
        assert_eq!(parsed.body, "Body line\n");
    }

    #[test]
    fn parses_one_nested_mapping() {
        let parsed = parse("---\nmetadata:\n  opencode/autoinvoke: yes\n---\nbody");
        assert!(parsed.error.is_none());
        assert_eq!(
            parsed.meta["metadata"]["opencode/autoinvoke"],
            Value::Bool(true)
        );
    }

    #[test]
    fn reports_unsupported_shapes() {
        assert!(
            parse("---\nmetadata:\n  nested:\n    deep: true\n---\n")
                .error
                .is_some()
        );
        assert!(parse("---\ntags: [a, b]\n---\n").error.is_some());
        assert!(parse("---\nname: unclosed\n").error.is_some());
    }

    #[test]
    fn keeps_numbers_for_numeric_fields() {
        let parsed = parse("---\nsteps: 1\nsubtask: 1\n---\n");
        assert_eq!(parsed.meta["steps"], 1);
        assert_eq!(parsed.meta["subtask"], 1);
        assert_eq!(
            super::coerce_bool(&parsed.meta["subtask"]),
            Some(true),
            "numeric booleans coerce where a boolean is expected"
        );
    }

    #[test]
    fn coerces_booleans() {
        assert_eq!(coerce_bool(&Value::String("on".to_owned())), Some(true));
        assert_eq!(coerce_bool(&Value::String("OFF".to_owned())), Some(false));
        assert_eq!(coerce_bool(&Value::from(1)), Some(true));
        assert_eq!(coerce_bool(&Value::from(2)), None);
    }
}
