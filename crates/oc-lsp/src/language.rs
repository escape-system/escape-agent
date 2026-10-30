//! File extension to LSP `languageId` mapping (docs/11-lsp.md).

use std::path::Path;

/// Map a file to the `languageId` sent with `textDocument/didOpen`.
///
/// Unknown extensions fall back to `plaintext`.
#[must_use]
pub fn language_id(path: &Path) -> &'static str {
    let file_name = path
        .file_name()
        .map(|name| name.to_string_lossy().to_ascii_lowercase())
        .unwrap_or_default();
    if file_name == "dockerfile" {
        return "dockerfile";
    }
    if file_name == "makefile" || file_name == "gnumakefile" {
        return "makefile";
    }
    let extension = path
        .extension()
        .map(|ext| ext.to_string_lossy().to_ascii_lowercase())
        .unwrap_or_default();
    match extension.as_str() {
        "ts" | "mts" | "cts" => "typescript",
        "tsx" => "typescriptreact",
        "js" | "mjs" | "cjs" => "javascript",
        "jsx" => "javascriptreact",
        "rs" => "rust",
        "py" | "pyi" => "python",
        "go" => "go",
        "java" => "java",
        "c" | "h" => "c",
        "cc" | "cpp" | "cxx" | "hpp" | "hh" | "hxx" => "cpp",
        "cs" => "csharp",
        "rb" | "rake" | "gemspec" => "ruby",
        "php" => "php",
        "sh" | "bash" | "zsh" => "shellscript",
        "yaml" | "yml" => "yaml",
        "json" | "jsonc" => "json",
        "md" | "markdown" => "markdown",
        "toml" => "toml",
        "lua" => "lua",
        "nix" => "nix",
        "tf" | "tfvars" => "terraform",
        "tex" | "bib" => "latex",
        "dockerfile" => "dockerfile",
        "gleam" => "gleam",
        "clj" | "cljs" | "cljc" | "edn" => "clojure",
        "hs" | "lhs" => "haskell",
        "kt" | "kts" => "kotlin",
        "dart" => "dart",
        "swift" => "swift",
        "zig" | "zon" => "zig",
        "vue" => "vue",
        "svelte" => "svelte",
        "astro" => "astro",
        "razor" | "cshtml" => "razor",
        "fs" | "fsi" | "fsx" => "fsharp",
        "ml" | "mli" => "ocaml",
        "ex" | "exs" => "elixir",
        "jl" => "julia",
        "typ" => "typst",
        "prisma" => "prisma",
        _ => "plaintext",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn maps_documented_extensions() {
        assert_eq!(language_id(Path::new("a.ts")), "typescript");
        assert_eq!(language_id(Path::new("a.tsx")), "typescriptreact");
        assert_eq!(language_id(Path::new("a.jsx")), "javascriptreact");
        assert_eq!(language_id(Path::new("a.mts")), "typescript");
        assert_eq!(language_id(Path::new("a.rs")), "rust");
        assert_eq!(language_id(Path::new("a.hpp")), "cpp");
        assert_eq!(language_id(Path::new("a.h")), "c");
        assert_eq!(language_id(Path::new("a.yml")), "yaml");
        assert_eq!(language_id(Path::new("Dockerfile")), "dockerfile");
        assert_eq!(language_id(Path::new("script.sh")), "shellscript");
    }

    #[test]
    fn unknown_extensions_fall_back_to_plaintext() {
        assert_eq!(language_id(Path::new("a.zzz")), "plaintext");
        assert_eq!(language_id(Path::new("Makefile")), "makefile");
    }
}
