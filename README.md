# Escape Agent (`opencode-rust`)

[English](#english) | [Türkçe](#türkçe)

---

<a name="english"></a>
## English

### Overview
**Escape Agent** is an enterprise-grade, high-performance, asynchronous Rust server and runtime designed for autonomous agent execution, interactive code assistance, and client applications (Web, CLI, and TUI). Built as a fully wire-compatible Rust reimplementation and extension of the Opencode server protocol, it pairs a native Rust backend with a tailored, modern glass-themed SolidJS frontend.

### Key Highlights & Capabilities
- **High-Performance Asynchronous Runtime**: Built on Tokio and Axum with strict zero-cost abstractions, zero `unsafe_code`, deterministic error handling, and robust crash-recovery semantics (survives process kills without data corruption).
- **Multi-Agent Orchestration & Lanes**: Hierarchical subagent spawning, concurrency control, isolated workspaces, and execution lanes (`oc-lanes` / `oc-subagents`).
- **Resilient Tool Execution**: Built-in, sandboxed execution of core engineering tools (`read`, `write`, `edit`, `patch`, `glob`, `grep`, `bash`/`shell`) with input size enforcement and output limits.
- **Protocol & Integration Support**:
  - **MCP (Model Context Protocol)**: stdio, HTTP, and OAuth client support.
  - **LSP (Language Server Protocol)**: Diagnostic integration and multi-language tool operations.
  - **Plugins**: Extensible native host interface with JavaScript sidecar bridges.
- **Autonomous Memory & Observer Loop**:
  - Persistent SQLite + FTS5 full-text search memory layer with auto-scribe capabilities.
  - Goal-tracking and self-healing observer loops (`oc-observer`) to detect and recover from task stalls or loops.
- **Branded Modern UI**: Pinned SolidJS single-page application customized with the **Escape Agent** dark/glass aesthetic, integrated SSE streaming, and Playwright verification suites.

### Workspace Architecture
The project is organized as a clean, modular Cargo workspace:

| Crate | Purpose |
|---|---|
| [`oc-types`](crates/oc-types) | Core wire protocols, typed event payloads, and API domain types |
| [`oc-store`](crates/oc-store) | SQLite-backed persistence, event log replay, and sequence allocation |
| [`oc-config`](crates/oc-config) | Configuration discovery, JSONC parsing, layered merging, and permission rules |
| [`oc-llm`](crates/oc-llm) | Streaming client integrations (OpenAI, Anthropic, Gemini, local providers) |
| [`oc-tools`](crates/oc-tools) | Sandboxed filesystem, search, patch, and shell tool executors |
| [`oc-runtime`](crates/oc-runtime) | Turn loop, actor sessions, prompt assembly, and auto-compaction |
| [`oc-lanes`](crates/oc-lanes) | Task classification, queue scheduling, and execution lanes |
| [`oc-subagents`](crates/oc-subagents) | Subagent lifecycle, permissions, and delegation hierarchy |
| [`oc-memory`](crates/oc-memory) | SQLite + FTS long-term memory retrieval and auto-scribe engine |
| [`oc-observer`](crates/oc-observer) | Expectation matching, goal tracking, and automated failure repair |
| [`oc-mcp`](crates/oc-mcp) | Model Context Protocol client (stdio / HTTP / OAuth) |
| [`oc-lsp`](crates/oc-lsp) | Language Server supervisor, diagnostics, and code intelligence |
| [`oc-plugin`](crates/oc-plugin) | Native host and JS sidecar plugin runtime |
| [`oc-pty`](crates/oc-pty) | Pseudo-terminal process daemon |
| [`oc-server`](crates/oc-server) | Axum web server, HTTP REST endpoints, Server-Sent Events (SSE), and static asset hosting |
| [`oc-cli`](crates/oc-cli) | Command-line interface and daemon supervisor (`opencode serve`) |
| [`oc-testkit`](crates/oc-testkit) | Offline test fixtures, mock servers, and scenario harnesses |

### Documentation
Detailed architecture and interface specifications are available in the [`docs/`](docs/README.md) directory:
- [00 — Vision & Requirements](docs/00-vision.md)
- [01 — Architecture & Crate Graph](docs/01-architecture.md)
- [02 — Protocol, Envelopes & SSE](docs/02-protocol.md)
- [03 — Storage & Crash Recovery](docs/03-storage.md)
- [04 — Agent Runtime & Turn Loop](docs/04-agent-runtime.md)
- [06 — Persistent Memory & Retrieval](docs/06-memory.md)
- [08 — Goals & Observer Loop](docs/08-goals-observer.md)
- [17 — Interfaces & Module Map](docs/17-interfaces.md)
- [18 — Live Implementation Status](docs/18-status.md)

### Getting Started

#### Prerequisites
- Rust (stable toolchain)
- Bun (for building the web UI)
- SQLite3

#### Building and Running the Server
```sh
# Build the workspace
cargo build

# Start the server on port 4096
cargo run -p oc-cli -- serve --port 4096

# Test server health and event stream
curl -s http://127.0.0.1:4096/api/info
curl -N http://127.0.0.1:4096/api/event
```

To run with authentication:
```sh
cargo run -p oc-cli -- serve --password secret
curl -s -u opencode:secret http://127.0.0.1:4096/api/info
```

#### Building the Web UI
```sh
ui/build.sh          # Linux/macOS
# or ui/build.ps1    # Windows
```
The compiled UI assets are written to `web/dist` and automatically served by `oc-server`.

#### Testing & Verification
```sh
# Run workspace unit and integration tests
cargo test --workspace

# Linting and formatting checks
cargo clippy --workspace --all-targets -- -D warnings
cargo fmt --all --check
```

---

<a name="türkçe"></a>
## Türkçe

### Genel Bakış
**Escape Agent**, otonom yazılım geliştirme ajanları, etkileşimli kod asistanları ve istemciler (Web, CLI ve TUI) için tasarlanmış yüksek performanslı, asenkron bir Rust sunucusu ve çalışma zamanıdır (runtime). Opencode sunucu protokolünün wire-uyumlu, bellek güvenli ve ölçeklenebilir bir Rust uygulamasıdır; modern koyu/cam (glass) temalı SolidJS web arayüzü ile birlikte çalışır.

### Temel Özellikler ve Yetenekler
- **Yüksek Performanslı Asenkron Mimari**: Tokio ve Axum üzerine kurulu; `unsafe_code` içermeyen, tamamen güvenli, deterministik hata yönetimi sunan ve ani kapanmalara (`kill -9`) karşı dayanıklı veri saklama mimarisi.
- **Çoklu Ajan ve Kulvar (Lanes) Yönetimi**: Alt ajanların hiyerarşik yönetimi, iş parçacığı/görev kulvarları (`oc-lanes`), eşzamanlılık kontrolü ve izole çalışma alanları.
- **Güvenli Araç Yürütme (Tools)**: Kod okuma, yazma, akıllı düzenleme (edit/patch), dosya arama (glob/grep) ve terminal/kabuk komutları için boyut kotalı ve izole araç altyapısı.
- **Protokol ve Genişleme Desteği**:
  - **MCP (Model Context Protocol)**: stdio, HTTP ve OAuth desteği.
  - **LSP (Language Server Protocol)**: Otomatik dil sunucusu yönetimi, kod teşhisleri ve akıllı navigasyon.
  - **Eklenti (Plugin) Sistemi**: Yerel Rust arayüzü ve JavaScript yan süreç (sidecar) köprüsü.
- **Kalıcı Bellek ve Gözlemci Döngüsü (Memory & Observer)**:
  - SQLite + FTS5 tam metin aramalı uzun vadeli bellek ve otomatik kayıt (auto-scribe) yeteneği.
  - Görev kilitlenmelerini, döngüleri ve hataları tespit edip kendi kendini onaran gözlemci döngüsü (`oc-observer`).
- **Özelleştirilmiş Modern Kullanıcı Arayüzü**: SolidJS tabanlı, **Escape Agent** marka kimliğiyle uyumlu koyu/cam temalı arayüz, SSE üzerinden gerçek zamanlı canlı akış ve Playwright uçtan uca testleri.

### Modüler Crate Mimarisi

| Crate | Açıklama |
|---|---|
| [`oc-types`](crates/oc-types) | Protokol veri modelleri, tiplenmiş olay yükleri ve API tanımları |
| [`oc-store`](crates/oc-store) | SQLite kalıcılık katmanı, olay günlüğü ve sıra yönetimi |
| [`oc-config`](crates/oc-config) | Konfigürasyon yönetimi, JSONC ayrıştırma ve izin hiyerarşisi |
| [`oc-llm`](crates/oc-llm) | LLM sağlayıcı entegrasyonları (OpenAI, Anthropic, Gemini vb.) |
| [`oc-tools`](crates/oc-tools) | Dosya sistemi, düzenleme, arama ve kabuk yürütücüleri |
| [`oc-runtime`](crates/oc-runtime) | Oturum aktörleri, tur döngüsü, prompt montajı ve mesaj sıkıştırma |
| [`oc-lanes`](crates/oc-lanes) | Görev sınıflandırma, kuyruk yönetimi ve yürütme kulvarları |
| [`oc-subagents`](crates/oc-subagents) | Alt ajan yaşam döngüsü, yetki denetimi ve delegasyon |
| [`oc-memory`](crates/oc-memory) | SQLite + FTS bellek arama ve otomatik hafıza kaydı |
| [`oc-observer`](crates/oc-observer) | Beklenti denetimi, hedef takibi ve otomatik onarım mekanizması |
| [`oc-mcp`](crates/oc-mcp) | Model Context Protocol istemcisi (stdio / HTTP / OAuth) |
| [`oc-lsp`](crates/oc-lsp) | Dil sunucusu gözetimi, tanılama ve kod zekası |
| [`oc-plugin`](crates/oc-plugin) | Yerel ve JS yan süreç tabanlı eklenti çalışma zamanı |
| [`oc-pty`](crates/oc-pty) | Sözde terminal (PTY) süreç yönetimi |
| [`oc-server`](crates/oc-server) | Axum tabanlı HTTP sunucusu, SSE akışı ve statik web sunumu |
| [`oc-cli`](crates/oc-cli) | Komut satırı arayüzü ve sunucu başlatıcı (`opencode serve`) |
| [`oc-testkit`](crates/oc-testkit) | Çevrimdışı test araçları, sahte sunucular ve senaryo testleri |

### Detaylı Dokümantasyon
Mimarinin derinlemesine incelendiği teknik şartnameler [`docs/`](docs/README.md) dizininde yer almaktadır:
- [00 — Vizyon ve Gereksinimler](docs/00-vision.md)
- [01 — Mimari ve Crate Grafiği](docs/01-architecture.md)
- [02 — Protokol ve SSE Arayüzü](docs/02-protocol.md)
- [03 — Kalıcı Depolama ve Kurtarma](docs/03-storage.md)
- [04 — Ajan Çalışma Zamanı (Runtime)](docs/04-agent-runtime.md)
- [06 — Kalıcı Bellek Katmanı](docs/06-memory.md)
- [08 — Hedefler ve Gözlemci Döngüsü](docs/08-goals-observer.md)
- [17 — Arayüzler ve Modül Haritası](docs/17-interfaces.md)
- [18 — Güncel Uygulama Durumu](docs/18-status.md)

### Başlarken

#### Gereksinimler
- Rust (güncel stable sürüm)
- Bun (Web arayüzünü derlemek için)
- SQLite3

#### Sunucuyu Derleme ve Çalıştırma
```sh
# Projeyi derleyin
cargo build

# Sunucuyu 4096 portunda başlatın
cargo run -p oc-cli -- serve --port 4096

# Bilgi ve SSE olay akışını test edin
curl -s http://127.0.0.1:4096/api/info
curl -N http://127.0.0.1:4096/api/event
```

Parola korumalı çalıştırma:
```sh
cargo run -p oc-cli -- serve --password secret
curl -s -u opencode:secret http://127.0.0.1:4096/api/info
```

#### Web Arayüzünü Derleme
```sh
ui/build.sh          # Linux/macOS
# veya ui/build.ps1  # Windows
```
Derlenen arayüz dosyaları `web/dist` içerisine aktarılır ve `oc-server` tarafından doğrudan sunulur.

#### Test ve Doğrulama
```sh
# Çalışma alanı birim ve entegrasyon testleri
cargo test --workspace

# Biçimlendirme ve statik analiz kontrolleri
cargo clippy --workspace --all-targets -- -D warnings
cargo fmt --all --check
```
