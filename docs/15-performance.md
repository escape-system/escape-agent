# 15 — Performance and memory

Budgets are release gates, not aspirations. Every number is measured by
`oc-bench` on a Release build, warm cache, Windows 11 x64 and Linux x64; a PR
that regresses a gate fails CI.

## Budgets

| Metric | Budget | Measurement |
|---|---|---|
| Listen→`/api/info` 200 (warm) | ≤ 200 ms | `oc-bench startup` |
| Listen→ready (cold, first run) | ≤ 1 s | fresh data dir, no network |
| Idle RSS, no session, no LSP, no plugins | ≤ 60 MB | process working set after 60 s |
| Idle RSS, one active session | ≤ 80 MB | +20 MB |
| Per additional location (config/agents/skills cached) | ≤ 2 MB | 20 locations |
| Per session in memory | ≤ 250 KB | 100 sessions, no transcript cache |
| Session working set during a turn | ≤ 8 MB | 5k-message session |
| SQLite page cache | ≤ 32 MB | `cache_size=-32768` |
| SSE per subscriber | ≤ 64 KB | 4,096-frame ring of refs + current frame |
| SSE fanout CPU per event | ≤ 1 ms p95, 100 subscribers | synthetic bus test |
| Durable commit | ≤ 2 ms p95, ≤ 10 ms p99 | WAL, NORMAL |
| Ephemeral delta latency (provider→SSE) | ≤ 20 ms p95 | local echo provider |
| Startup network calls | 0 on critical path | trace assert |
| Idle CPU | < 0.5 % of one core | 5 min sample |
| History load per step | O(new rows), ≤ 2 ms p95 | 5k-message replay |
| LSP idle RSS | 0 when unused; ≤ 1 server default | `max_servers` |
| Plugin sidecar | 0 unless configured; ≤ 90 MB | configured/no plugins |
| MCP children | 0 until first use if `lazy_mcp`; ≤ 8 | config |
| PTY buffer | ≤ 1 MB/terminal, ≤ 16 exited retained | daemon |

## Mechanism map

| Problem | Mechanism |
|---|---|
| Upstream copies the 7.8 MB model catalog per location | one `Arc<Catalog>` globally; locations hold `Arc` + a small overlay |
| 55-service graph per location | `Location` is a plain struct: config snapshot, agent map, skill index, MCP handles; built lazily, ≤ 50 ms warm |
| Full transcript re-read per step | per-session watermark (`through_seq`) + decoded tail; only new rows are read |
| Every part re-serialized on each event | parts are written once at `*.ended`; ordinal diff only for changed rows |
| JSON per subscriber | serialize once to `Bytes`, fan out `Arc<Bytes>`/`Bytes::clone` |
| Per-delta DB writes | deltas are ephemeral; one transaction per durable event |
| Snapshot capture twice per step | capture once per step, reuse the tree id for diffs; no git call when nothing changed |
| npm/network at boot | no network in the critical path; catalog snapshot embedded; refresh in background |
| Eager MCP/LSP | lazy connect; bounded concurrency |
| Watcher explosion | ≤ 1 recursive watcher per config root, ≤ 256 file watches/location, TTL sweep |
| Search index rebuild every 10 s | incremental index, 4 MB/location cap, one shared worker |
| Resume storm | ≤ 8 concurrent resumes at boot, FIFO queue |
| Allocator fragmentation | `mimalloc` on Windows/Linux |
| Tokio oversubscription | worker threads `min(cores, 8)`, blocking pool only for SQLite/fs |

## Allocation rules (compiled code)

- No `.clone()` of a message/part/event payload on the hot path. Use `Arc` or
  move. A clone is acceptable only at a durable boundary.
- Streaming: read provider chunks into a reusable buffer; do not `format!` per
  delta; text deltas are `&str` slices appended to a `String` once.
- Serialize deltas directly into a pre-sized `Vec<u8>`; skip serialization for
  subscribers whose `types` filter excludes the event (check before encode).
- `serde_json::value::RawValue` for opaque config/persisted payloads: never
  parse what we only store and return.
- File reads: `read` tool streams into a bounded buffer; never load a 2 GB file
  to truncate it.
- Base64: encode only when the client did not negotiate `experimental-media-uri`.
- Avoid `spawn` per tool call for trivial tools; use the step `JoinSet`.
- Bounded everything: `HashMap` → `lru`/`moka` with a documented cap; no
  unbounded `Vec` keyed by session or event.

## Hot paths and their tests

| Path | Test |
|---|---|
| `POST /prompt` → first SSE delta | mock provider, 1 tool call, assert p95 |
| 50 concurrent SSE subscribers, 1k events | fanout CPU + RSS + overflow behavior |
| 5k-message session, 10 steps | history load time stays flat |
| 100 sessions idle, 1 active | RSS ≤ 80 MB |
| 20 locations | RSS delta ≤ 40 MB |
| Crash at every durable boundary | restart → no duplicate/missing events |
| 10k memory rows | FTS recall ≤ 3 ms |
| 8 subagents, 3 deep | observer tick ≤ 1 ms CPU (no LLM call) |
| LSP spawn/evict × 20 | no zombie processes, RSS returns to baseline |
| MCP server crash × 10 | no reconnect storm, tools drop once |

## Benchmark harness

- `oc-bench` is a separate binary in `oc-testkit`, not shipped.
- Fake provider: local HTTP server that streams deterministic chunks with
  configurable delays; no API keys in CI.
- `startup`: spawn with a temp data dir, poll `/api/info` every 1 ms, record
  first 200 and ready; run 20×, report p50/p95.
- `rss`: sample the OS counter (Windows `GetProcessMemoryInfo`, Linux
  `/proc/self/status`) after forced GC-equivalent (drop caches) and 30 s idle.
- `sse`: one process, N subscribers, emit M events, measure wall time and max
  RSS; assert the 4096 overflow closes exactly the slow subscriber.
- `history`: seed SQLite directly, then run steps through the fake provider.
- Results are stored as JSON and compared against `bench/baseline.json`; a >10 %
  regression fails unless the PR carries a `perf:` justification.

## What we will not do

- Cache full transcripts in RAM to hit latency; correctness across revert/fork
  and memory blowup are worse.
- Hold a SQLite connection across a provider call.
- Run the JS sidecar by default "just in case".
- Bundle an embedding model or a language server.
- Trade durability for speed on durable events (terminal events after commit).
