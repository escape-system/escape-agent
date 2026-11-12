//! HTML to text/markdown: comment stripping, tag scanning, entity decoding and
//! whitespace collapsing.

const SKIP_TAGS: [&str; 8] = [
    "script", "style", "noscript", "iframe", "object", "embed", "template", "svg",
];

const BLOCK_TAGS: [&str; 33] = [
    "address",
    "article",
    "aside",
    "blockquote",
    "body",
    "br",
    "dd",
    "div",
    "dl",
    "dt",
    "fieldset",
    "figcaption",
    "figure",
    "footer",
    "form",
    "h1",
    "h2",
    "h3",
    "h4",
    "h5",
    "h6",
    "head",
    "header",
    "hr",
    "li",
    "main",
    "nav",
    "ol",
    "p",
    "pre",
    "section",
    "table",
    "ul",
];

/// Strip HTML to readable text: tags, scripts and styles removed, entities
/// decoded, whitespace collapsed, and block elements split across lines.
pub fn html_to_text(html: &str) -> String {
    render_html(html, Render::Text)
}

/// Conservative markdown fallback: the same text rendering with `#` heading
/// prefixes and `- ` list bullets. Links, emphasis, and tables are flattened.
pub fn html_to_markdown(html: &str) -> String {
    render_html(html, Render::Markdown)
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Render {
    Text,
    Markdown,
}

fn render_html(html: &str, mode: Render) -> String {
    let html = strip_comments(html);
    let mut output = String::new();
    let mut skip_depth = 0_usize;
    let mut rest = html.as_str();
    while !rest.is_empty() {
        if !rest.starts_with('<') {
            let Some(character) = rest.chars().next() else {
                break;
            };
            if character == '&'
                && let Some((decoded, consumed)) = decode_entity(rest)
            {
                if skip_depth == 0 {
                    output.push(decoded);
                }
                rest = &rest[consumed..];
                continue;
            }
            if skip_depth == 0 {
                output.push(character);
            }
            rest = &rest[character.len_utf8()..];
            continue;
        }

        let Some(end) = rest.find('>') else {
            break;
        };
        let inner = &rest[1..end];
        rest = &rest[end + 1..];
        if inner.starts_with('!') || inner.starts_with('?') || inner.starts_with("!--") {
            continue;
        }
        let (closing, name, self_closing) = parse_tag(inner);
        if name.is_empty() {
            continue;
        }
        let skip_tag = SKIP_TAGS.contains(&name.as_str());
        if skip_depth > 0 {
            if skip_tag {
                if closing {
                    skip_depth -= 1;
                } else if !self_closing {
                    skip_depth += 1;
                }
            }
            if skip_depth == 0 {
                push_break(&mut output);
            }
            continue;
        }
        if skip_tag {
            if !closing && !self_closing {
                skip_depth = 1;
            }
            continue;
        }
        if !BLOCK_TAGS.contains(&name.as_str()) {
            continue;
        }
        if !closing && mode == Render::Markdown {
            if let Some(level) = heading_level(&name) {
                push_break(&mut output);
                output.push_str("#".repeat(level).as_str());
                output.push(' ');
                continue;
            }
            if name == "li" {
                push_break(&mut output);
                output.push_str("- ");
                continue;
            }
        }
        push_break(&mut output);
    }

    finalize_text(&output)
}

fn strip_comments(html: &str) -> String {
    let mut result = String::with_capacity(html.len());
    let mut rest = html;
    while let Some(start) = rest.find("<!--") {
        result.push_str(&rest[..start]);
        let after = &rest[start + 4..];
        match after.find("-->") {
            Some(end) => rest = &after[end + 3..],
            None => return result,
        }
    }
    result.push_str(rest);
    result
}

/// Split `<tag ...>` inner text into `(closing, lowercase name, self-closing)`.
fn parse_tag(inner: &str) -> (bool, String, bool) {
    let trimmed = inner.trim_start();
    let (closing, body) = match trimmed.strip_prefix('/') {
        Some(body) => (true, body),
        None => (false, trimmed),
    };
    let name_end = body
        .find(|character: char| character.is_whitespace() || character == '/')
        .unwrap_or(body.len());
    let name = body[..name_end].to_ascii_lowercase();
    let self_closing = body.trim_end().ends_with('/');
    (closing, name, self_closing)
}

fn heading_level(name: &str) -> Option<usize> {
    match name {
        "h1" => Some(1),
        "h2" => Some(2),
        "h3" => Some(3),
        "h4" => Some(4),
        "h5" => Some(5),
        "h6" => Some(6),
        _ => None,
    }
}

fn push_break(output: &mut String) {
    while output.ends_with(' ') || output.ends_with('\t') {
        output.pop();
    }
    if !output.is_empty() && !output.ends_with('\n') {
        output.push('\n');
    }
}

fn finalize_text(text: &str) -> String {
    let mut result = String::new();
    for line in text.split('\n') {
        let collapsed = line.split_whitespace().collect::<Vec<_>>().join(" ");
        if collapsed.is_empty() {
            continue;
        }
        if !result.is_empty() {
            result.push('\n');
        }
        result.push_str(&collapsed);
    }
    result
}

/// Decode an HTML entity at the start of `text`; returns the character and the
/// number of bytes consumed.
fn decode_entity(text: &str) -> Option<(char, usize)> {
    let end = text.find(';')?;
    if end > 34 {
        return None;
    }
    let body = &text[1..end];
    if body.is_empty() {
        return None;
    }
    let decoded = if let Some(hex) = body.strip_prefix("#x").or_else(|| body.strip_prefix("#X")) {
        u32::from_str_radix(hex, 16).ok().and_then(char::from_u32)
    } else if let Some(decimal) = body.strip_prefix('#') {
        decimal.parse::<u32>().ok().and_then(char::from_u32)
    } else {
        named_entity(body)
    }?;
    Some((decoded, end + 1))
}

fn named_entity(name: &str) -> Option<char> {
    let decoded = match name {
        "amp" => '&',
        "lt" => '<',
        "gt" => '>',
        "quot" => '"',
        "apos" => '\'',
        "nbsp" => ' ',
        "copy" => '\u{a9}',
        "reg" => '\u{ae}',
        "trade" => '\u{2122}',
        "mdash" => '\u{2014}',
        "ndash" => '\u{2013}',
        "hellip" => '\u{2026}',
        "lsquo" => '\u{2018}',
        "rsquo" => '\u{2019}',
        "ldquo" => '\u{201c}',
        "rdquo" => '\u{201d}',
        "laquo" => '\u{ab}',
        "raquo" => '\u{bb}',
        "middot" => '\u{b7}',
        "bull" => '\u{2022}',
        "deg" => '\u{b0}',
        "plusmn" => '\u{b1}',
        "times" => '\u{d7}',
        "divide" => '\u{f7}',
        "euro" => '\u{20ac}',
        "pound" => '\u{a3}',
        "yen" => '\u{a5}',
        "cent" => '\u{a2}',
        "sect" => '\u{a7}',
        "para" => '\u{b6}',
        _ => return None,
    };
    Some(decoded)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strips_script_and_style_without_leaking_content() {
        let html = "<html><head><style>p{color:red}</style><script>alert('x')</script></head>\
                    <body><h1>Title</h1><p>Hello &amp; welcome</p></body></html>";
        let text = html_to_text(html);
        assert!(!text.contains("alert"));
        assert!(!text.contains("color:red"));
        assert!(!text.contains('<'));
        assert!(text.contains("Title"));
        assert!(text.contains("Hello & welcome"));
    }

    #[test]
    fn markdown_fallback_adds_heading_and_list_markers() {
        let html = "<h2>Section</h2><ul><li>One</li><li>Two</li></ul><p>Plain</p>";
        let markdown = html_to_markdown(html);
        assert!(markdown.contains("## Section"), "{markdown}");
        assert!(markdown.contains("- One"), "{markdown}");
        assert!(markdown.contains("- Two"), "{markdown}");
    }

    #[test]
    fn decodes_numeric_and_named_entities() {
        let text = html_to_text("<p>&#65;&#x42;&lt;tag&gt; a&mdash;b &copy;</p>");
        assert_eq!(text, "AB<tag> a\u{2014}b \u{a9}");
    }
}
