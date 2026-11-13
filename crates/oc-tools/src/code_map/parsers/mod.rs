//! Per-language symbol extraction. Each parser scans one line at a time and never
//! depends on another language's rules.

use crate::code_map::Symbol;

mod c_cpp;
mod go;
mod markdown;
mod python;
mod rust;
mod ts_js;

use c_cpp::parse_c_cpp_symbols;
use go::parse_go_symbols;
use markdown::parse_markdown_headings;
use python::parse_python_symbols;
use rust::parse_rust_symbols;
use ts_js::parse_ts_js_symbols;

/// Pure-Rust multi-language parser for signatures and symbols.
#[must_use]
pub fn parse_symbols(content: &str, ext: &str) -> Vec<Symbol> {
    let clean = content.strip_prefix('\u{feff}').unwrap_or(content);
    match ext.to_lowercase().as_str() {
        "rs" => parse_rust_symbols(clean),
        "ts" | "tsx" | "js" | "jsx" | "mjs" | "cjs" => parse_ts_js_symbols(clean),
        "py" => parse_python_symbols(clean),
        "go" => parse_go_symbols(clean),
        "c" | "cpp" | "cc" | "h" | "hpp" => parse_c_cpp_symbols(clean),
        "md" => parse_markdown_headings(clean),
        _ => Vec::new(),
    }
}

pub(super) fn extract_ident(input: &str) -> String {
    input
        .chars()
        .take_while(|c| c.is_alphanumeric() || *c == '_')
        .collect()
}

pub(super) fn clean_signature(line: &str) -> String {
    let trimmed = line.trim();
    let without_brace = if let Some(stripped) = trimmed.strip_suffix('{') {
        stripped.trim()
    } else {
        trimmed
    };
    if without_brace.len() > 140 {
        // Truncate on a char boundary: a byte index of 137 can land inside a
        // multi-byte character and panic the slice.
        let mut end = 137;
        while end > 0 && !without_brace.is_char_boundary(end) {
            end -= 1;
        }
        format!("{}...", &without_brace[..end])
    } else {
        without_brace.to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::code_map::{SymbolKind, Visibility};

    #[test]
    fn parses_rust_symbols_with_visibility_and_docs() {
        let src = r#"
/// Service context for managing sessions.
pub struct SessionService {
    id: String,
}

pub trait SessionManager {
    fn run(&self);
}

impl SessionManager for SessionService {
    pub async fn run(&self) {
        println!("running");
    }
}

fn private_helper() -> bool {
    true
}
"#;
        let symbols = parse_rust_symbols(src);
        assert_eq!(symbols.len(), 6);

        assert_eq!(symbols[0].name, "SessionService");
        assert_eq!(symbols[0].kind, SymbolKind::Struct);
        assert_eq!(symbols[0].visibility, Visibility::Public);
        assert!(symbols[0].doc.as_ref().unwrap().contains("Service context"));

        assert_eq!(symbols[1].name, "SessionManager");
        assert_eq!(symbols[1].kind, SymbolKind::Trait);

        assert_eq!(symbols[2].name, "run");
        assert_eq!(symbols[2].kind, SymbolKind::Function);

        assert_eq!(symbols[3].name, "SessionManager for SessionService");
        assert_eq!(symbols[3].kind, SymbolKind::Class);

        assert_eq!(symbols[4].name, "run");
        assert_eq!(symbols[4].kind, SymbolKind::Function);
        assert_eq!(symbols[4].visibility, Visibility::Public);

        assert_eq!(symbols[5].name, "private_helper");
        assert_eq!(symbols[5].kind, SymbolKind::Function);
        assert_eq!(symbols[5].visibility, Visibility::Private);
    }

    #[test]
    fn parses_ts_and_python_symbols() {
        let ts_src = r#"
export interface UserSession { id: string; }
export class EngineRunner extends Runner {
    constructor() {}
}
export async function startEngine(port: number): Promise<void> {}
const secretKey = "xyz";
"#;
        let ts_symbols = parse_ts_js_symbols(ts_src);
        assert_eq!(ts_symbols.len(), 3);
        assert_eq!(ts_symbols[0].name, "UserSession");
        assert_eq!(ts_symbols[0].kind, SymbolKind::Interface);
        assert_eq!(ts_symbols[1].name, "EngineRunner");
        assert_eq!(ts_symbols[1].kind, SymbolKind::Class);
        assert_eq!(ts_symbols[2].name, "startEngine");
        assert_eq!(ts_symbols[2].kind, SymbolKind::Function);

        let py_src = r"
class Controller:
    def handle_request(self, req):
        pass

async def serve_forever():
    pass
";
        let py_symbols = parse_python_symbols(py_src);
        assert_eq!(py_symbols.len(), 3);
        assert_eq!(py_symbols[0].name, "Controller");
        assert_eq!(py_symbols[0].kind, SymbolKind::Class);
        assert_eq!(py_symbols[1].name, "handle_request");
        assert_eq!(py_symbols[1].kind, SymbolKind::Method);
        assert_eq!(py_symbols[2].name, "serve_forever");
        assert_eq!(py_symbols[2].kind, SymbolKind::Function);
    }

    /// A long signature whose 137th byte falls inside a multi-byte character
    /// must truncate on a char boundary instead of panicking.
    #[test]
    fn clean_signature_truncates_on_a_char_boundary() {
        // 46 x 3-byte chars = 138 bytes, so byte 137 lands mid-character.
        let long = format!("pub fn {}x()", "…".repeat(46));
        assert!(long.len() > 140);
        assert!(!long.is_char_boundary(137));

        let signature = clean_signature(&long);
        assert!(signature.ends_with("..."));
        assert!(signature.len() <= 140);
        assert!(long.starts_with(signature.trim_end_matches("...")));
    }
}
