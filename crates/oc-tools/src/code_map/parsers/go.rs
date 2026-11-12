//! Go `func`/`type` scanning, including receiver methods.

use super::{clean_signature, extract_ident};
use crate::code_map::{Symbol, SymbolKind, Visibility};

pub(super) fn parse_go_symbols(content: &str) -> Vec<Symbol> {
    let mut symbols = Vec::new();

    for (idx, line) in content.lines().enumerate() {
        let trimmed = line.trim();
        if let Some(after) = trimmed.strip_prefix("func ") {
            let (kind, name) = if after.starts_with('(') {
                // Method: func (r *Receiver) Name(...)
                let method_name = after
                    .split(')')
                    .nth(1)
                    .map_or_else(|| "method".to_string(), |s| extract_ident(s.trim()));
                (SymbolKind::Method, method_name)
            } else {
                (SymbolKind::Function, extract_ident(after))
            };
            let vis = if name.chars().next().is_some_and(char::is_uppercase) {
                Visibility::Public
            } else {
                Visibility::Private
            };
            symbols.push(Symbol {
                name,
                kind,
                signature: clean_signature(trimmed),
                line: idx + 1,
                doc: None,
                visibility: vis,
            });
        } else if let Some(after) = trimmed.strip_prefix("type ") {
            let name = extract_ident(after);
            let kind = if trimmed.contains("interface") {
                SymbolKind::Interface
            } else {
                SymbolKind::Struct
            };
            let vis = if name.chars().next().is_some_and(char::is_uppercase) {
                Visibility::Public
            } else {
                Visibility::Private
            };
            symbols.push(Symbol {
                name,
                kind,
                signature: clean_signature(trimmed),
                line: idx + 1,
                doc: None,
                visibility: vis,
            });
        }
    }

    symbols
}
