//! C/C++ `class`/`struct` scanning.

use super::{clean_signature, extract_ident};
use crate::code_map::{Symbol, SymbolKind, Visibility};

pub(super) fn parse_c_cpp_symbols(content: &str) -> Vec<Symbol> {
    let mut symbols = Vec::new();

    for (idx, line) in content.lines().enumerate() {
        let trimmed = line.trim();
        if let Some(after) = trimmed.strip_prefix("class ") {
            let name = extract_ident(after);
            symbols.push(Symbol {
                name,
                kind: SymbolKind::Class,
                signature: clean_signature(trimmed),
                line: idx + 1,
                doc: None,
                visibility: Visibility::Public,
            });
        } else if let Some(after) = trimmed.strip_prefix("struct ") {
            let name = extract_ident(after);
            symbols.push(Symbol {
                name,
                kind: SymbolKind::Struct,
                signature: clean_signature(trimmed),
                line: idx + 1,
                doc: None,
                visibility: Visibility::Public,
            });
        }
    }

    symbols
}
