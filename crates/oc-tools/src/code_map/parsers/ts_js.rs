//! TypeScript/JavaScript declarations.

use super::{clean_signature, extract_ident};
use crate::code_map::{Symbol, SymbolKind, Visibility};

pub(super) fn parse_ts_js_symbols(content: &str) -> Vec<Symbol> {
    let mut symbols = Vec::new();

    for (idx, line) in content.lines().enumerate() {
        let trimmed = line.trim();
        if trimmed.is_empty()
            || trimmed.starts_with("//")
            || trimmed.starts_with("/*")
            || trimmed.starts_with('*')
        {
            continue;
        }

        let is_export = trimmed.starts_with("export ");
        let vis = if is_export {
            Visibility::Public
        } else {
            Visibility::Private
        };
        let rest = trimmed
            .trim_start_matches("export")
            .trim_start_matches("default")
            .trim();

        if let Some(after) = rest.strip_prefix("function ") {
            let name = extract_ident(after);
            symbols.push(Symbol {
                name,
                kind: SymbolKind::Function,
                signature: clean_signature(trimmed),
                line: idx + 1,
                doc: None,
                visibility: vis,
            });
        } else if let Some(after) = rest.strip_prefix("async function ") {
            let name = extract_ident(after);
            symbols.push(Symbol {
                name,
                kind: SymbolKind::Function,
                signature: clean_signature(trimmed),
                line: idx + 1,
                doc: None,
                visibility: vis,
            });
        } else if let Some(after) = rest.strip_prefix("class ") {
            let name = extract_ident(after);
            symbols.push(Symbol {
                name,
                kind: SymbolKind::Class,
                signature: clean_signature(trimmed),
                line: idx + 1,
                doc: None,
                visibility: vis,
            });
        } else if let Some(after) = rest.strip_prefix("interface ") {
            let name = extract_ident(after);
            symbols.push(Symbol {
                name,
                kind: SymbolKind::Interface,
                signature: clean_signature(trimmed),
                line: idx + 1,
                doc: None,
                visibility: vis,
            });
        } else if let Some(after) = rest.strip_prefix("type ") {
            let name = extract_ident(after);
            symbols.push(Symbol {
                name,
                kind: SymbolKind::Type,
                signature: clean_signature(trimmed),
                line: idx + 1,
                doc: None,
                visibility: vis,
            });
        } else if is_export && (rest.starts_with("const ") || rest.starts_with("let ")) {
            let after = rest
                .trim_start_matches("const ")
                .trim_start_matches("let ")
                .trim();
            let name = extract_ident(after);
            symbols.push(Symbol {
                name,
                kind: SymbolKind::Function,
                signature: clean_signature(trimmed),
                line: idx + 1,
                doc: None,
                visibility: vis,
            });
        }
    }

    symbols
}
