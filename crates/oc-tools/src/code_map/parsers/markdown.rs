//! Markdown heading outline.

use crate::code_map::{Symbol, SymbolKind, Visibility};

pub(super) fn parse_markdown_headings(content: &str) -> Vec<Symbol> {
    let mut symbols = Vec::new();

    for (idx, line) in content.lines().enumerate() {
        let trimmed = line.trim();
        if trimmed.starts_with('#') {
            let level = trimmed.chars().take_while(|c| *c == '#').count();
            let text = trimmed.trim_start_matches('#').trim();
            symbols.push(Symbol {
                name: text.to_string(),
                kind: SymbolKind::Heading,
                signature: format!("{} {}", "#".repeat(level), text),
                line: idx + 1,
                doc: None,
                visibility: Visibility::Public,
            });
        }
    }

    symbols
}
