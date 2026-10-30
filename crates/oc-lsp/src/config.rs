//! LSP configuration: a built-in server table merged with user config.
//!
//! There is deliberately no downloader. A built-in server is only usable when
//! its command resolves on `PATH` or in the project's `node_modules/.bin`;
//! otherwise the supervisor reports it as unavailable.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use serde_json::Value;

/// Maximum number of language-server clients one supervisor may keep alive.
pub const DEFAULT_MAX_SERVERS: usize = 4;
/// Global slot budget; heavy servers occupy two slots.
pub const GLOBAL_SLOT_BUDGET: usize = 6;
/// Servers that dominate memory and therefore cost two slots.
pub const HEAVY_SERVERS: &[&str] = &["rust", "jdtls", "kotlin-ls", "clangd", "csharp"];

/// One resolved LSP server definition.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LspServerConfig {
    /// Stable id used in the registry and event payloads.
    pub id: String,
    /// Display name.
    pub name: String,
    /// Command and arguments. Empty means "not configured".
    #[serde(default)]
    pub command: Vec<String>,
    /// Extensions (`.rs`) or exact filenames (`Dockerfile`). Empty matches all
    /// files (upstream v2 semantics).
    #[serde(default)]
    pub extensions: Vec<String>,
    /// Disabled servers are never spawned.
    #[serde(default)]
    pub disabled: bool,
    /// Environment variables merged over the inherited process environment.
    #[serde(default)]
    pub env: BTreeMap<String, String>,
    /// `initializationOptions` and `workspace/didChangeConfiguration` settings.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub initialization: Option<Value>,
}

impl LspServerConfig {
    /// Whether this server is one of the memory-heavy ones.
    #[must_use]
    pub fn is_heavy(&self) -> bool {
        HEAVY_SERVERS.contains(&self.id.as_str())
    }

    /// Slot cost used by the supervisor's budget accounting.
    #[must_use]
    pub fn slot_cost(&self) -> usize {
        if self.is_heavy() { 2 } else { 1 }
    }

    /// Whether the file matches this server's extension/filename list.
    #[must_use]
    pub fn matches_file(&self, path: &Path) -> bool {
        if self.extensions.is_empty() {
            return true;
        }
        let file_name = path
            .file_name()
            .map(|name| name.to_string_lossy().to_ascii_lowercase())
            .unwrap_or_default();
        let extension = path
            .extension()
            .map(|ext| format!(".{}", ext.to_string_lossy().to_ascii_lowercase()));
        self.extensions.iter().any(|candidate| {
            let candidate = candidate.to_ascii_lowercase();
            if candidate.starts_with('.') {
                extension.as_deref() == Some(candidate.as_str())
            } else {
                file_name == candidate
            }
        })
    }

    /// Resolve the command for `root`, returning an argv whose first element is
    /// an absolute executable path.
    pub fn resolve_command(&self, root: &Path) -> Result<Vec<String>, String> {
        let Some(program) = self.command.first() else {
            return Err("no command configured".into());
        };
        let executable = resolve_executable(program, root)
            .ok_or_else(|| format!("`{program}` was not found on PATH or in node_modules/.bin"))?;
        let mut argv = Vec::with_capacity(self.command.len());
        argv.push(executable.to_string_lossy().into_owned());
        argv.extend(self.command.iter().skip(1).cloned());
        Ok(argv)
    }
}

/// Supervisable LSP configuration.
#[derive(Debug, Clone)]
pub struct LspConfig {
    /// `lsp: false` turns the subsystem off.
    pub enabled: bool,
    /// Opt-in install manifest flag; the downloader itself is not implemented.
    pub install: bool,
    /// Maximum concurrent clients.
    pub max_servers: usize,
    /// Active server definitions in built-in table order, then custom ids.
    pub servers: Vec<LspServerConfig>,
}

impl Default for LspConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            install: false,
            max_servers: DEFAULT_MAX_SERVERS,
            servers: builtin_servers(),
        }
    }
}

impl LspConfig {
    /// Merge a top-level config document (`lsp`, `lsp_install`) over the
    /// built-in table.
    ///
    /// Accepted shapes:
    /// - `"lsp": false` / `true`
    /// - `"lsp": {"servers": {id: {...}}, "install": bool, "maxServers": n}`
    /// - `"lsp": {id: {...}}` (a bare server map, per docs/14)
    #[must_use]
    pub fn from_config(document: &Value) -> Self {
        let mut config = Self::default();

        match document.get("lsp") {
            Some(Value::Bool(false)) => config.enabled = false,
            Some(Value::Object(object)) => {
                config.enabled = object
                    .get("enabled")
                    .and_then(Value::as_bool)
                    .unwrap_or(true);
                if let Some(install) = object.get("install").and_then(Value::as_bool) {
                    config.install = install;
                }
                if let Some(max) = object
                    .get("maxServers")
                    .or_else(|| object.get("max_servers"))
                    .and_then(Value::as_u64)
                {
                    config.max_servers = usize::try_from(max).unwrap_or(config.max_servers).max(1);
                }
                if let Some(servers) = object.get("servers").and_then(Value::as_object) {
                    config.merge_servers(servers);
                } else {
                    let looks_like_server = |value: &Value| {
                        let Some(entry) = value.as_object() else {
                            return false;
                        };
                        entry.contains_key("command")
                            || entry.contains_key("extensions")
                            || entry.contains_key("disabled")
                            || entry.contains_key("initialization")
                            || entry.contains_key("env")
                    };
                    let bare: serde_json::Map<String, Value> = object
                        .iter()
                        .filter(|(key, value)| {
                            !matches!(
                                key.as_str(),
                                "enabled" | "install" | "maxServers" | "max_servers"
                            ) && looks_like_server(value)
                        })
                        .map(|(key, value)| (key.clone(), value.clone()))
                        .collect();
                    config.merge_servers(&bare);
                }
            }
            _ => {}
        }

        if let Some(servers) = document.get("lsp.servers").and_then(Value::as_object) {
            config.merge_servers(servers);
        }
        if let Some(install) = document.get("lsp_install").and_then(Value::as_bool) {
            config.install = install;
        }
        if download_disabled() {
            config.install = false;
        }
        config
    }

    /// Overlay configured servers by id.
    pub fn merge_servers(&mut self, servers: &serde_json::Map<String, Value>) {
        for (id, value) in servers {
            let Some(entry) = value.as_object() else {
                continue;
            };
            if let Some(existing) = self.servers.iter_mut().find(|server| server.id == *id) {
                merge_entry(existing, entry);
                continue;
            }
            let mut server = custom_server(id, entry);
            server.disabled = entry
                .get("disabled")
                .and_then(Value::as_bool)
                .unwrap_or(false);
            self.servers.push(server);
        }
    }

    /// Look up a server by id.
    #[must_use]
    pub fn get(&self, id: &str) -> Option<&LspServerConfig> {
        self.servers.iter().find(|server| server.id == id)
    }

    /// Servers eligible for spawning (`disabled: false` and a non-empty
    /// command).
    #[must_use]
    pub fn active_servers(&self) -> Vec<&LspServerConfig> {
        self.servers
            .iter()
            .filter(|server| !server.disabled && !server.command.is_empty())
            .collect()
    }

    /// Servers matching `path`, in table order.
    #[must_use]
    pub fn servers_for_file(&self, path: &Path) -> Vec<&LspServerConfig> {
        self.active_servers()
            .into_iter()
            .filter(|server| server.matches_file(path))
            .collect()
    }
}

fn merge_entry(server: &mut LspServerConfig, entry: &serde_json::Map<String, Value>) {
    if let Some(argv) = entry.get("command").and_then(command_argv) {
        server.command = argv;
    }
    if let Some(extensions) = entry.get("extensions").and_then(Value::as_array) {
        server.extensions = extensions
            .iter()
            .filter_map(Value::as_str)
            .map(str::to_ascii_lowercase)
            .collect();
    }
    if let Some(disabled) = entry.get("disabled").and_then(Value::as_bool) {
        server.disabled = disabled;
    }
    if let Some(env) = entry.get("env").and_then(Value::as_object) {
        for (key, value) in env {
            if let Some(value) = value.as_str() {
                server.env.insert(key.clone(), value.to_owned());
            }
        }
    }
    if let Some(initialization) = entry.get("initialization") {
        server.initialization = Some(initialization.clone());
    }
}

fn custom_server(id: &str, entry: &serde_json::Map<String, Value>) -> LspServerConfig {
    let command = entry
        .get("command")
        .and_then(command_argv)
        .unwrap_or_default();
    let extensions = entry
        .get("extensions")
        .and_then(Value::as_array)
        .map(|values| {
            values
                .iter()
                .filter_map(Value::as_str)
                .map(str::to_ascii_lowercase)
                .collect()
        })
        .unwrap_or_default();
    let mut env = BTreeMap::new();
    if let Some(values) = entry.get("env").and_then(Value::as_object) {
        for (key, value) in values {
            if let Some(value) = value.as_str() {
                env.insert(key.clone(), value.to_owned());
            }
        }
    }
    LspServerConfig {
        id: id.to_owned(),
        name: entry
            .get("name")
            .and_then(Value::as_str)
            .map_or_else(|| id.to_owned(), str::to_owned),
        command,
        extensions,
        disabled: false,
        env,
        initialization: entry.get("initialization").cloned(),
    }
}

fn command_argv(value: &Value) -> Option<Vec<String>> {
    match value {
        Value::Array(items) => Some(
            items
                .iter()
                .filter_map(Value::as_str)
                .map(str::to_owned)
                .collect(),
        ),
        Value::String(text) => Some(text.split_whitespace().map(str::to_owned).collect()),
        _ => None,
    }
}

fn download_disabled() -> bool {
    std::env::var("OPENCODE_DISABLE_LSP_DOWNLOAD").is_ok_and(|value| {
        !value.is_empty() && value != "0" && !value.eq_ignore_ascii_case("false")
    })
}

/// Resolve `program` to an executable file, searching the project's
/// `node_modules/.bin` first and then `PATH`.
#[must_use]
pub fn resolve_executable(program: &str, root: &Path) -> Option<PathBuf> {
    if program.is_empty() {
        return None;
    }
    let looks_like_path = program.contains('/') || program.contains('\\');
    if looks_like_path {
        let candidate = Path::new(program);
        let candidate = if candidate.is_absolute() {
            candidate.to_path_buf()
        } else {
            root.join(candidate)
        };
        return candidate.is_file().then_some(candidate);
    }

    let mut directories = Vec::new();
    let mut cursor = Some(root);
    while let Some(directory) = cursor {
        directories.push(directory.join("node_modules").join(".bin"));
        cursor = directory.parent();
    }
    if let Some(path) = std::env::var_os("PATH") {
        directories.extend(std::env::split_paths(&path));
    }

    let candidates = executable_names(program);
    for directory in directories {
        for name in &candidates {
            let candidate = directory.join(name);
            if candidate.is_file() {
                return Some(candidate);
            }
        }
    }
    None
}

fn executable_names(program: &str) -> Vec<String> {
    let mut names = vec![program.to_owned()];
    if cfg!(windows) {
        let already_has_extension = Path::new(program).extension().is_some();
        if !already_has_extension {
            let path_ext =
                std::env::var("PATHEXT").unwrap_or_else(|_| ".COM;.EXE;.BAT;.CMD".into());
            for extension in path_ext.split(';').filter(|part| !part.is_empty()) {
                names.push(format!("{program}{}", extension.to_ascii_lowercase()));
            }
        }
    }
    names
}

/// Nearest project root for a language server, found by walking up from the
/// file towards `workspace_root`.
#[must_use]
pub fn resolve_root(server_id: &str, file: &Path, workspace_root: &Path) -> PathBuf {
    let markers = root_markers(server_id);
    let workspace_root = canonical_or_owned(workspace_root);
    let start = file.parent().map(canonical_or_owned);
    let mut cursor = start.as_deref();
    while let Some(directory) = cursor {
        if has_marker(directory, markers) {
            return directory.to_path_buf();
        }
        if directory == workspace_root {
            break;
        }
        cursor = directory.parent();
    }
    workspace_root
}

fn has_marker(directory: &Path, markers: &[&str]) -> bool {
    markers.iter().any(|marker| {
        if let Some(file) = marker.strip_suffix("[workspace]") {
            // `Cargo.toml[workspace]`: only a manifest that declares a
            // workspace roots rust-analyzer; member manifests are skipped so
            // one server serves the whole workspace.
            return std::fs::read_to_string(directory.join(file)).is_ok_and(|text| {
                text.lines()
                    .any(|line| line.trim_start().starts_with("[workspace"))
            });
        }
        if let Some(suffix) = marker.strip_prefix('*') {
            return std::fs::read_dir(directory).is_ok_and(|entries| {
                entries.filter_map(Result::ok).any(|entry| {
                    entry
                        .file_name()
                        .to_string_lossy()
                        .to_ascii_lowercase()
                        .ends_with(&suffix.to_ascii_lowercase())
                })
            });
        }
        directory.join(marker).exists()
    })
}

fn canonical_or_owned(path: &Path) -> PathBuf {
    std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf())
}

fn root_markers(server_id: &str) -> &'static [&'static str] {
    match server_id {
        "rust" => &["Cargo.toml[workspace]"],
        "deno" => &["deno.json", "deno.jsonc"],
        "typescript" | "vue" | "svelte" | "astro" | "eslint" | "oxlint" | "biome" => {
            &["tsconfig.json", "jsconfig.json", "package.json"]
        }
        "gopls" => &["go.mod"],
        "pyright" | "ty" => &[
            "pyproject.toml",
            "setup.py",
            "setup.cfg",
            "requirements.txt",
            "pyrightconfig.json",
        ],
        "ruby-lsp" => &["Gemfile", ".ruby-version"],
        "elixir-ls" => &["mix.exs"],
        "zls" => &["build.zig"],
        "csharp" | "fsharp" | "razor" => &["*.csproj", "*.fsproj", "*.sln"],
        "jdtls" => &[
            "pom.xml",
            "build.gradle",
            "build.gradle.kts",
            "settings.gradle",
            ".git",
        ],
        "kotlin-ls" => &["build.gradle.kts", "settings.gradle.kts", "build.gradle"],
        "dart" => &["pubspec.yaml"],
        "ocaml-lsp" => &["dune-project", "dune"],
        "terraform" => &[".terraform", "*.tf"],
        "gleam" => &["gleam.toml"],
        "clojure-lsp" => &["deps.edn", "project.clj"],
        "nixd" => &["flake.nix", "default.nix"],
        "haskell-language-server" => &["*.cabal", "stack.yaml", "cabal.project"],
        "julials" => &["Project.toml", "JuliaProject.toml"],
        _ => &[".git"],
    }
}

/// Built-in table: ids only carry defaults; nothing is downloaded.
#[must_use]
pub fn builtin_servers() -> Vec<LspServerConfig> {
    BUILTINS
        .iter()
        .map(|(id, name, command, extensions)| LspServerConfig {
            id: (*id).to_owned(),
            name: (*name).to_owned(),
            command: command.iter().map(|part| (*part).to_owned()).collect(),
            extensions: extensions.iter().map(|ext| (*ext).to_owned()).collect(),
            disabled: false,
            env: BTreeMap::new(),
            initialization: None,
        })
        .collect()
}

type Builtin = (
    &'static str,
    &'static str,
    &'static [&'static str],
    &'static [&'static str],
);

const BUILTINS: &[Builtin] = &[
    (
        "deno",
        "Deno",
        &["deno", "lsp"],
        &[".ts", ".tsx", ".js", ".jsx", ".mjs", ".cjs"],
    ),
    (
        "typescript",
        "TypeScript",
        &["typescript-language-server", "--stdio"],
        &[".ts", ".tsx", ".mts", ".cts", ".js", ".jsx", ".mjs", ".cjs"],
    ),
    ("vue", "Vue", &["vue-language-server", "--stdio"], &[".vue"]),
    (
        "eslint",
        "ESLint",
        &["vscode-eslint-language-server", "--stdio"],
        &[".js", ".jsx", ".mjs", ".cjs", ".ts", ".tsx", ".vue"],
    ),
    (
        "oxlint",
        "Oxlint",
        &["oxlint", "--lsp"],
        &[".js", ".jsx", ".ts", ".tsx", ".vue"],
    ),
    (
        "biome",
        "Biome",
        &["biome", "lsp-proxy"],
        &[".js", ".jsx", ".ts", ".tsx", ".json", ".jsonc"],
    ),
    ("gopls", "gopls", &["gopls"], &[".go"]),
    (
        "ruby-lsp",
        "Ruby LSP",
        &["ruby-lsp"],
        &[".rb", ".rake", ".gemspec"],
    ),
    ("ty", "ty", &["ty", "server"], &[".py", ".pyi"]),
    (
        "pyright",
        "Pyright",
        &["pyright-langserver", "--stdio"],
        &[".py", ".pyi"],
    ),
    ("elixir-ls", "ElixirLS", &["elixir-ls"], &[".ex", ".exs"]),
    ("zls", "ZLS", &["zls"], &[".zig", ".zon"]),
    ("csharp", "C#", &["csharp-ls"], &[".cs"]),
    ("razor", "Razor", &["rzls"], &[".razor", ".cshtml"]),
    (
        "fsharp",
        "F#",
        &["fsautocomplete"],
        &[".fs", ".fsi", ".fsx"],
    ),
    (
        "sourcekit-lsp",
        "SourceKit-LSP",
        &["sourcekit-lsp"],
        &[".swift", ".m", ".mm"],
    ),
    ("rust", "rust-analyzer", &["rust-analyzer"], &[".rs"]),
    (
        "clangd",
        "clangd",
        &["clangd"],
        &[".c", ".cc", ".cpp", ".cxx", ".h", ".hh", ".hpp", ".hxx"],
    ),
    (
        "svelte",
        "Svelte",
        &["svelteserver", "--stdio"],
        &[".svelte"],
    ),
    ("astro", "Astro", &["astro-ls", "--stdio"], &[".astro"]),
    ("jdtls", "Eclipse JDT", &["jdtls"], &[".java"]),
    (
        "kotlin-ls",
        "Kotlin",
        &["kotlin-language-server"],
        &[".kt", ".kts"],
    ),
    (
        "yaml-ls",
        "YAML",
        &["yaml-language-server", "--stdio"],
        &[".yaml", ".yml"],
    ),
    ("lua-ls", "Lua", &["lua-language-server"], &[".lua"]),
    (
        "php-intelephense",
        "Intelephense",
        &["intelephense", "--stdio"],
        &[".php"],
    ),
    (
        "prisma",
        "Prisma",
        &["prisma-language-server", "--stdio"],
        &[".prisma"],
    ),
    ("dart", "Dart", &["dart", "language-server"], &[".dart"]),
    ("ocaml-lsp", "OCaml", &["ocamllsp"], &[".ml", ".mli"]),
    (
        "bash",
        "Bash",
        &["bash-language-server", "start"],
        &[".sh", ".bash", ".zsh"],
    ),
    (
        "terraform",
        "Terraform",
        &["terraform-ls", "serve"],
        &[".tf", ".tfvars"],
    ),
    ("texlab", "TexLab", &["texlab"], &[".tex", ".bib"]),
    (
        "dockerfile",
        "Dockerfile",
        &["docker-langserver", "--stdio"],
        &[".dockerfile", "Dockerfile"],
    ),
    ("gleam", "Gleam", &["gleam", "lsp"], &[".gleam"]),
    (
        "clojure-lsp",
        "Clojure LSP",
        &["clojure-lsp", "serve"],
        &[".clj", ".cljs", ".cljc", ".edn"],
    ),
    ("nixd", "nixd", &["nixd"], &[".nix"]),
    ("tinymist", "Tinymist", &["tinymist"], &[".typ"]),
    (
        "haskell-language-server",
        "Haskell",
        &["haskell-language-server-wrapper", "--lsp"],
        &[".hs", ".lhs"],
    ),
    (
        "julials",
        "Julia",
        &[
            "julia",
            "--startup-file=no",
            "--history-file=no",
            "-e",
            "using LanguageServer; runserver()",
        ],
        &[".jl"],
    ),
];

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn builtin_table_has_the_documented_ids() {
        let ids: Vec<&str> = BUILTINS.iter().map(|entry| entry.0).collect();
        assert_eq!(ids.len(), 38);
        for required in [
            "deno",
            "typescript",
            "gopls",
            "rust",
            "clangd",
            "jdtls",
            "julials",
        ] {
            assert!(ids.contains(&required), "missing {required}");
        }
    }

    #[test]
    fn disabled_entries_are_removed_from_active_servers() {
        let config = LspConfig::from_config(&json!({
            "lsp": {"servers": {"rust": {"disabled": true}}}
        }));
        assert!(config.get("rust").unwrap().disabled);
        assert!(
            config
                .active_servers()
                .iter()
                .all(|server| server.id != "rust")
        );
    }

    #[test]
    fn command_and_extensions_override_builtins() {
        let config = LspConfig::from_config(&json!({
            "lsp": {
                "servers": {
                    "rust": {
                        "command": ["/opt/ra", "--log-file", "/tmp/ra.log"],
                        "extensions": [".rs", ".rs.in"],
                        "env": {"RUST_LOG": "info"},
                        "initialization": {"checkOnSave": true}
                    }
                }
            }
        }));
        let rust = config.get("rust").unwrap();
        assert_eq!(rust.command, ["/opt/ra", "--log-file", "/tmp/ra.log"]);
        assert_eq!(rust.extensions, vec![".rs", ".rs.in"]);
        assert_eq!(rust.env.get("RUST_LOG").map(String::as_str), Some("info"));
        assert_eq!(rust.initialization, Some(json!({"checkOnSave": true})));
    }

    #[test]
    fn custom_server_requires_command_to_be_active() {
        let config = LspConfig::from_config(&json!({
            "lsp": {"servers": {
                "mine": {"command": ["my-ls"], "extensions": [".foo"]},
                "empty": {"extensions": [".bar"]}
            }}
        }));
        assert!(config.get("mine").is_some());
        assert!(config.get("empty").is_some());
        assert_eq!(config.active_servers().len(), 1 + 38);
        assert!(config.get("mine").unwrap().matches_file(Path::new("x.foo")));
    }

    #[test]
    fn lsp_false_disables_everything_but_keeps_registry() {
        let config = LspConfig::from_config(&json!({"lsp": false, "lsp_install": true}));
        assert!(!config.enabled);
        assert_eq!(config.servers.len(), 38);
    }

    #[test]
    fn literal_dotted_servers_key_is_accepted() {
        let config = LspConfig::from_config(&json!({
            "lsp": true,
            "lsp.servers": {
                "custom": {"command": ["custom-ls"], "extensions": [".custom"]}
            }
        }));
        assert!(config.get("custom").is_some());
        assert!(
            config
                .get("custom")
                .unwrap()
                .matches_file(Path::new("a.custom"))
        );
    }

    #[test]
    fn empty_extensions_match_every_file() {
        let server = LspServerConfig {
            id: "x".into(),
            name: "x".into(),
            command: vec!["x".into()],
            extensions: vec![],
            disabled: false,
            env: BTreeMap::new(),
            initialization: None,
        };
        assert!(server.matches_file(Path::new("anything.zzz")));
        assert!(server.matches_file(Path::new("Makefile")));
    }

    #[test]
    fn exact_filenames_match_extensionless_files() {
        let config = LspConfig::default();
        let docker = config.get("dockerfile").unwrap();
        assert!(docker.matches_file(Path::new("Dockerfile")));
        assert!(docker.matches_file(Path::new("nested/Dockerfile")));
        assert!(!docker.matches_file(Path::new("Dockerfile.dev")));
    }

    #[test]
    fn heavy_servers_cost_two_slots() {
        let config = LspConfig::default();
        assert_eq!(config.get("rust").unwrap().slot_cost(), 2);
        assert_eq!(config.get("typescript").unwrap().slot_cost(), 1);
    }

    #[test]
    fn root_resolution_walks_to_the_nearest_marker() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path();
        std::fs::create_dir_all(root.join("member/src")).unwrap();
        std::fs::write(
            root.join("Cargo.toml"),
            "[workspace]\nmembers = [\"member\"]",
        )
        .unwrap();
        std::fs::write(
            root.join("member/Cargo.toml"),
            "[package]\nname = \"member\"",
        )
        .unwrap();
        let file = root.join("member/src/lib.rs");
        std::fs::write(&file, "fn main() {}").unwrap();
        let resolved = resolve_root("rust", &file, root);
        // A member manifest does not root rust-analyzer; the workspace does.
        assert_eq!(resolved, std::fs::canonicalize(root).unwrap());
    }

    #[test]
    fn standalone_crate_with_a_workspace_manifest_roots_itself() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path();
        std::fs::create_dir_all(root.join("src")).unwrap();
        std::fs::write(root.join("Cargo.toml"), "[workspace]\nmembers = [\".\"]").unwrap();
        let file = root.join("src/main.rs");
        std::fs::write(&file, "fn main() {}").unwrap();
        assert_eq!(
            resolve_root("rust", &file, root),
            std::fs::canonicalize(root).unwrap()
        );
    }

    #[test]
    fn root_falls_back_to_workspace_root() {
        let temp = tempfile::tempdir().unwrap();
        let root = std::fs::canonicalize(temp.path()).unwrap();
        let file = root.join("odd.zzz");
        std::fs::write(&file, "").unwrap();
        assert_eq!(resolve_root("unknown", &file, &root), root);
    }
}
