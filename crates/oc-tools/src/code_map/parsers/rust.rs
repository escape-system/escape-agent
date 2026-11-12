//! Rust item scanning: qualifiers, visibility and `///` doc accumulation.

use super::{clean_signature, extract_ident};
use crate::code_map::{Symbol, SymbolKind, Visibility};

fn strip_rust_qualifiers(mut s: &str) -> (&str, Visibility) {
    let mut vis = Visibility::Private;
    loop {
        s = s.trim_start();
        if let Some(rest) = s.strip_prefix("pub(crate)") {
            s = rest;
            vis = Visibility::Public;
        } else if let Some(rest) = s.strip_prefix("pub(super)") {
            s = rest;
            vis = Visibility::Public;
        } else if let Some(rest) = s.strip_prefix("pub ") {
            s = rest;
            vis = Visibility::Public;
        } else if let Some(rest) = s.strip_prefix("async ") {
            s = rest;
        } else if let Some(rest) = s.strip_prefix("const ") {
            s = rest;
        } else if let Some(rest) = s.strip_prefix("unsafe ") {
            s = rest;
        } else if let Some(rest) = s.strip_prefix("extern ") {
            s = rest;
        } else {
            break;
        }
    }
    (s.trim_start(), vis)
}

pub(super) fn parse_rust_symbols(content: &str) -> Vec<Symbol> {
    let mut symbols = Vec::new();
    let mut doc_acc: Option<String> = None;

    for (idx, line) in content.lines().enumerate() {
        let trimmed = line.trim();
        if trimmed.starts_with("///") || trimmed.starts_with("//!") {
            let doc_text = trimmed
                .trim_start_matches("///")
                .trim_start_matches("//!")
                .trim();
            doc_acc = Some(
                doc_acc.map_or_else(|| doc_text.to_string(), |acc| format!("{acc} {doc_text}")),
            );
            continue;
        }

        if trimmed.is_empty()
            || trimmed.starts_with("//")
            || trimmed.starts_with("/*")
            || trimmed.starts_with('*')
        {
            continue;
        }

        let (rest, vis) = strip_rust_qualifiers(trimmed);

        if let Some(after) = rest.strip_prefix("fn ") {
            let name = extract_ident(after);
            symbols.push(Symbol {
                name,
                kind: SymbolKind::Function,
                signature: clean_signature(trimmed),
                line: idx + 1,
                doc: doc_acc.take(),
                visibility: vis,
            });
        } else if let Some(after) = rest.strip_prefix("struct ") {
            let name = extract_ident(after);
            symbols.push(Symbol {
                name,
                kind: SymbolKind::Struct,
                signature: clean_signature(trimmed),
                line: idx + 1,
                doc: doc_acc.take(),
                visibility: vis,
            });
        } else if let Some(after) = rest.strip_prefix("enum ") {
            let name = extract_ident(after);
            symbols.push(Symbol {
                name,
                kind: SymbolKind::Enum,
                signature: clean_signature(trimmed),
                line: idx + 1,
                doc: doc_acc.take(),
                visibility: vis,
            });
        } else if let Some(after) = rest.strip_prefix("trait ") {
            let name = extract_ident(after);
            symbols.push(Symbol {
                name,
                kind: SymbolKind::Trait,
                signature: clean_signature(trimmed),
                line: idx + 1,
                doc: doc_acc.take(),
                visibility: vis,
            });
        } else if let Some(after) = rest.strip_prefix("type ") {
            let name = extract_ident(after);
            symbols.push(Symbol {
                name,
                kind: SymbolKind::Type,
                signature: clean_signature(trimmed),
                line: idx + 1,
                doc: doc_acc.take(),
                visibility: vis,
            });
        } else if let Some(after) = rest.strip_prefix("mod ") {
            let name = extract_ident(after);
            symbols.push(Symbol {
                name,
                kind: SymbolKind::Module,
                signature: clean_signature(trimmed),
                line: idx + 1,
                doc: doc_acc.take(),
                visibility: vis,
            });
        } else if let Some(after) = rest.strip_prefix("impl ") {
            let name_str = clean_signature(after);
            symbols.push(Symbol {
                name: name_str,
                kind: SymbolKind::Class,
                signature: clean_signature(trimmed),
                line: idx + 1,
                doc: doc_acc.take(),
                visibility: vis,
            });
        } else {
            doc_acc = None;
        }
    }

    symbols
}
