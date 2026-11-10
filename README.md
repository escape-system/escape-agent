<p align="center">
  <img src="assets/logo.png" alt="Escape Agent Logo" width="128" />
</p>

# Escape Agent (`opencode-rust`)

[English](#english) | [Türkçe](#türkçe)

---

<a name="english"></a>
## English

### Overview
**Escape Agent** is a high-performance, asynchronous Rust server and runtime designed for autonomous agent execution, interactive code assistance, and developer workflows. Built as a wire-compatible Rust reimplementation and extension of core OpenCode server protocols, it pairs a native Rust backend with a branded, glass-themed SolidJS web client.

> **Status:** Active Developer Preview (Alpha). Core session lifecycle, tools, persistent memory, and observer loops are fully functional with 800+ automated tests. Advanced features (such as Code Mode, V1 session imports, and JS sidecar bridge execution) are under active development and return truthful 501/503 statuses.

### Key Capabilities & Highlights
- **High-Performance Asynchronous Runtime**: Built on Tokio and Axum with zero `unsafe_code`, deterministic error handling, and crash-resilient persistence (survives process kills without state corruption).
- **Multi-Agent Orchestration & Lanes**: Hierarchical subagent spawning, concurrency control, isolated workspaces, and execution lanes (`oc-lanes` / `oc-subagents`).
- **Resilient Tool Sandbox**: Sandboxed execution of core engineering tools (`read`, `write`, `edit`, `patch`, `glob`, `grep`, `shell`) with strict input size enforcement and output limits.
- **Protocol & Integration Support**:
  - **MCP (Model Context Protocol)**: stdio, HTTP, and OAuth client support.
  - **LSP (Language Server Protocol)**: Diagnostic integration and multi-language tool operations.
  - **Plugins**: Extensible native host interface with JavaScript sidecar bridge contracts.
- **Autonomous Memory & Observer Loop**:
  - Persistent SQLite + FTS5 full-text search memory layer with auto-scribe capabilities.
  - Goal-tracking and self-healing observer loops (`oc-observer`) to detect and recover from task stalls or loops.
- **Tailored Modern Web UI**: Adapted from OpenCode's SolidJS client with custom Escape Agent aurora/glass aesthetics, integrated SSE streaming, and Playwright verification suites.
- **Conflict-Free Storage**: Stores SQLite state in a dedicated directory (`~/.local/share/escape-agent/escape.db`), preventing database or migration collisions with upstream OpenCode.

### Workspace Architecture
The project is organized as a modular Cargo workspace:

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
- [20 — Engineering Guide](docs/20-handoff.md)

### Prebuilt Binaries

Ready-made builds are attached to [GitHub Releases](https://github.com/escape-system/escape-agent/releases)
as `opencode-<version>-<os>-<arch>.tar.gz` plus a `.sha256` checksum sidecar; see
[docs/23-release.md](docs/23-release.md) for the archive layout.

### Getting Started

#### Prerequisites
- Rust (stable toolchain, 1.90+)
- Bun (for building the web UI)

#### Building and Running the Server
```sh
# Build the workspace
cargo build

# Start the server on default port 4096
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
# Run workspace unit and integration tests (800+ tests)
cargo test --workspace

# Linting and formatting checks
cargo clippy --workspace --all-targets -- -D warnings
cargo fmt --all --check
```

### License
This project is licensed under the [MIT License](LICENSE). Portions of the web UI and wire contracts are adapted from [OpenCode](https://github.com/opencode-ai/opencode) under the MIT License.

---

<a name="türkçe"></a>
## Türkçe

### Genel Bakış
**Escape Agent**, otonom yazılım geliştirme ajanları, etkileşimli kod asistanları ve geliştirici iş akışları için tasarlanmış yüksek performanslı, asenkron bir Rust sunucusu ve çalışma zamanıdır (runtime). Temel OpenCode sunucu protokollerinin wire-uyumlu, bellek güvenli ve modüler bir Rust uygulamasıdır; koyu/cam (aurora/glass) temalı SolidJS web arayüzü ile birlikte çalışır.

> **Durum:** Geliştirici Önizlemesi (Alfa). Oturum yaşam döngüsü, araçlar, kalıcı bellek ve gözlemci döngüleri 800+ otomatik test ile doğrulanmış olarak çalışmaktadır. Henüz tamamlanmamış gelişmiş özellikler (Code Mode, V1 oturum aktarımı, JS sidecar bridge çalıştırması vb.) aktif geliştirme aşamasındadır ve gerçeğe uygun şekilde 501/503 yanıtları döndürür.

### Temel Özellikler ve Yetenekler
- **Yüksek Performanslı Asenkron Mimari**: Tokio ve Axum üzerine kurulu; `unsafe_code` içermeyen, deterministik hata yönetimi sunan ve ani süreç sonlandırmalarına (`kill -9`) karşı dayanıklı kalıcılık mimarisi.
- **Çoklu Ajan ve Kulvar (Lanes) Yönetimi**: Alt ajanların hiyerarşik yönetimi, iş parçacığı/görev kulvarları (`oc-lanes`), eşzamanlılık kontrolü ve izole çalışma alanları.
- **Güvenli Araç Yürütme (Tools)**: Kod okuma, yazma, akıllı düzenleme (edit/patch), dosya arama (glob/grep) ve kabuk komutları için boyut kotalı ve güvenli araç altyapısı.
- **Protokol ve Genişleme Desteği**:
  - **MCP (Model Context Protocol)**: stdio, HTTP ve OAuth desteği.
  - **LSP (Language Server Protocol)**: Dil sunucusu gözetimi, tanılama ve kod zekası.
  - **Eklenti (Plugin) Sistemi**: Yerel Rust arayüzü ve JavaScript yan süreç (sidecar) köprü sözleşmeleri.
- **Kalıcı Bellek ve Gözlemci Döngüsü (Memory & Observer)**:
  - SQLite + FTS5 tam metin aramalı uzun vadeli bellek ve otomatik kayıt (auto-scribe) yeteneği.
  - Görev kilitlenmelerini, döngüleri ve hataları tespit edip kendi kendini onaran gözlemci döngüsü (`oc-observer`).
- **Özelleştirilmiş Modern Web Arayüzü**: OpenCode SolidJS istemcisi temel alınarak hazırlanan Escape Agent aurora/cam temalı arayüz, SSE üzerinden canlı akış ve Playwright testleri.
- **Çakışmasız Veri Depolama**: Varsayılan SQLite veritabanı ayrılmış bir dizinde (`~/.local/share/escape-agent/escape.db`) saklanır; upstream OpenCode şemalarıyla çakışma yaşanmaz.

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
- [20 — Mühendislik Rehberi](docs/20-handoff.md)

### Hazır Derlenmiş Paketler

Hazır derlenmiş sürümler [GitHub Releases](https://github.com/escape-system/escape-agent/releases)
sayfasında `opencode-<sürüm>-<işletim sistemi>-<mimari>.tar.gz` ve `.sha256` sağlama toplamı
dosyasıyla yayınlanır; arşiv düzeni için [docs/23-release.md](docs/23-release.md).

### Başlarken

#### Gereksinimler
- Rust (güncel stable sürüm, 1.90+)
- Bun (Web arayüzünü derlemek için)

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
# Çalışma alanı birim ve entegrasyon testleri (800+ test)
cargo test --workspace

# Biçimlendirme ve statik analiz kontrolleri
cargo clippy --workspace --all-targets -- -D warnings
cargo fmt --all --check
```

### Lisans
Bu proje [MIT Lisansı](LICENSE) altında lisanslanmıştır. Web arayüzü ve protokol sözleşmelerinin ilgili kısımları MIT Lisansı kapsamında [OpenCode](https://github.com/opencode-ai/opencode) projesinden uyarlanmıştır.
