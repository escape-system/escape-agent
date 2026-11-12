//! Python `def`/`class` scanning, including indentation-based method detection.

use super::{clean_signature, extract_ident};
use crate::code_map::{Symbol, SymbolKind, Visibility};

pub(super) fn parse_python_symbols(content: &str) -> Vec<Symbol> {
    let mut symbols = Vec::new();

    for (idx, line) in content.lines().enumerate() {
        let trimmed = line.trim();
        if trimmed.starts_with("def ") || trimmed.starts_with("async def ") {
            let after = trimmed
                .trim_start_matches("async ")
                .trim_start_matches("def ")
                .trim();
            let name = extract_ident(after);
            let is_method = line.starts_with("    ") || line.starts_with('\t');
            let vis = if name.starts_with('_') {
                Visibility::Private
            } else {
                Visibility::Public
            };
            symbols.push(Symbol {
                name,
                kind: if is_method {
                    SymbolKind::Method
                } else {
                    SymbolKind::Function
                },
                signature: clean_signature(trimmed),
                line: idx + 1,
                doc: None,
                visibility: vis,
            });
        } else if let Some(after) = trimmed.strip_prefix("class ") {
            let name = extract_ident(after);
            let vis = if name.starts_with('_') {
                Visibility::Private
            } else {
                Visibility::Public
            };
            symbols.push(Symbol {
                name,
                kind: SymbolKind::Class,
                signature: clean_signature(trimmed),
                line: idx + 1,
                doc: None,
                visibility: vis,
            });
        }
    }

    symbols
}
