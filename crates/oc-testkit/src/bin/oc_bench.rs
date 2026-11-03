//! `oc-bench` — the performance gate binary promised by `docs/15-performance.md`.
//!
//! The document states that budgets are release gates measured by `oc-bench`
//! on a Release build. This binary is that gate. It measures the subset of the
//! budget table that is observable from outside the server process:
//!
//! | gate | budget | source |
//! |---|---|---|
//! | `startup.warm.listen_p95` | ≤ 200 ms | `docs/15-performance.md:11` |
//! | `startup.cold.spawn_to_info` | ≤ 1 s | `docs/15-performance.md:12` |
//! | `idle_rss.vmrss` | ≤ 60 MB | `docs/15-performance.md:13` |
//! | `history.page.p95` | ≤ 2 ms | `docs/15-performance.md:25` |
//! | `history.walk.p95` | ≤ 2 ms | `docs/15-performance.md:25` |
//!
//! The tool must tell the truth even about its own product: a measured value
//! above its documented budget is reported as `UNMET` and makes the process
//! exit non-zero. Budgets are never relaxed to turn a gate green.
//!
//! Usage:
//!
//! ```text
//! oc-bench <startup|idle-rss|history|all> [options]
//!
//!   --pid <PID>            measure an already-running server (idle-rss only)
//!   --binary <PATH>        server binary; default: sibling of this executable
//!   --iterations <N>       warm startup samples (default 20)
//!   --idle-seconds <N>     idle time before the RSS sample (default 60)
//!   --messages <N>         transcript size for `history` (default 5000)
//! ```

use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use anyhow::{Context, bail};
use base64::Engine as _;
use base64::engine::general_purpose::STANDARD as BASE64;
use oc_store::{NewMessage, NewSession, Store};
use oc_testkit::{ServerHarness, TestConfig};
use reqwest::Client;
use serde_json::{Value, json};

/// `docs/15-performance.md:11` — Listen→`/api/info` 200 (warm).
const BUDGET_STARTUP_WARM_MS: f64 = 200.0;
/// `docs/15-performance.md:12` — Listen→ready (cold, first run).
const BUDGET_STARTUP_COLD_MS: f64 = 1000.0;
/// `docs/15-performance.md:13` — Idle RSS, no session, no LSP, no plugins.
const BUDGET_IDLE_RSS_MB: f64 = 60.0;
/// `docs/15-performance.md:25` — History load per step.
const BUDGET_HISTORY_MS: f64 = 2.0;

const PASSWORD: &str = "oc-bench-password";
const SESSION_ID: &str = "ses_ocbenchhistory0000000000";
const PROJECT_ID: &str = "prj_ocbench";
const INFO_TIMEOUT: Duration = Duration::from_secs(30);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Command {
    Startup,
    IdleRss,
    History,
    All,
    Help,
}

#[derive(Debug)]
struct Args {
    command: Command,
    pid: Option<u32>,
    binary: Option<PathBuf>,
    iterations: usize,
    idle_seconds: u64,
    messages: usize,
}

impl Default for Args {
    fn default() -> Self {
        Self {
            command: Command::Help,
            pid: None,
            binary: None,
            iterations: 20,
            idle_seconds: 60,
            messages: 5000,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Status {
    Pass,
    Unmet,
    Skip,
}

impl Status {
    fn label(self) -> &'static str {
        match self {
            Self::Pass => "PASS",
            Self::Unmet => "UNMET",
            Self::Skip => "SKIP",
        }
    }
}

#[derive(Debug)]
struct Metric {
    name: &'static str,
    value: f64,
    unit: &'static str,
    budget: f64,
    budget_unit: &'static str,
    source: &'static str,
    status: Status,
    detail: String,
}

impl Metric {
    fn gate(
        name: &'static str,
        value: f64,
        unit: &'static str,
        budget: f64,
        budget_unit: &'static str,
        source: &'static str,
        detail: String,
    ) -> Self {
        let status = if value > budget {
            Status::Unmet
        } else {
            Status::Pass
        };
        Self {
            name,
            value,
            unit,
            budget,
            budget_unit,
            source,
            status,
            detail,
        }
    }

    fn skip(
        name: &'static str,
        budget: f64,
        budget_unit: &'static str,
        source: &'static str,
        detail: impl Into<String>,
    ) -> Self {
        Self {
            name,
            value: f64::NAN,
            unit: "",
            budget,
            budget_unit,
            source,
            status: Status::Skip,
            detail: detail.into(),
        }
    }
}

#[tokio::main]
async fn main() -> ExitCode {
    let args = match parse_args(std::env::args().skip(1)) {
        Ok(args) => args,
        Err(error) => {
            eprintln!("oc-bench: {error}");
            eprintln!("try `oc-bench --help`");
            return ExitCode::from(2);
        }
    };
    if args.command == Command::Help {
        println!("{USAGE}");
        return ExitCode::SUCCESS;
    }

    match run(args).await {
        Ok(metrics) => {
            print_report(&metrics);
            if metrics.iter().any(|metric| metric.status == Status::Unmet) {
                ExitCode::FAILURE
            } else {
                ExitCode::SUCCESS
            }
        }
        Err(error) => {
            eprintln!("oc-bench: {error:#}");
            ExitCode::from(2)
        }
    }
}

async fn run(args: Args) -> anyhow::Result<Vec<Metric>> {
    let mut metrics = Vec::new();
    match args.command {
        Command::Startup => {
            let binary = resolve_binary(args.binary.as_deref())?;
            metrics.extend(measure_startup(&binary, args.iterations).await?);
        }
        Command::IdleRss => {
            let binary = match args.pid {
                Some(_) => None,
                None => Some(resolve_binary(args.binary.as_deref())?),
            };
            metrics.extend(measure_idle_rss(binary.as_deref(), args.pid, args.idle_seconds).await?);
        }
        Command::History => {
            let binary = resolve_binary(args.binary.as_deref())?;
            metrics.extend(measure_history(&binary, args.messages, args.iterations).await?);
        }
        Command::All => {
            let binary = resolve_binary(args.binary.as_deref())?;
            println!(
                "oc-bench: server binary {} ({} bytes)\n",
                binary.display(),
                std::fs::metadata(&binary).map_or(0, |meta| meta.len())
            );
            metrics.extend(measure_startup(&binary, args.iterations).await?);
            metrics.extend(measure_idle_rss(Some(&binary), None, args.idle_seconds).await?);
            metrics.extend(measure_history(&binary, args.messages, args.iterations).await?);
        }
        Command::Help => {}
    }
    Ok(metrics)
}

// --- startup ---------------------------------------------------------------

async fn measure_startup(binary: &Path, iterations: usize) -> anyhow::Result<Vec<Metric>> {
    let client = build_client()?;
    let config = TestConfig::new().with_env("OPENCODE_SERVER_PASSWORD", PASSWORD);

    // Warm: the same data directory is reused, so migrations and the config
    // cache are already in place. Each sample restarts the real process.
    let mut harness = ServerHarness::prepare(binary, config.clone())?;
    let mut listen_ms = Vec::with_capacity(iterations);
    let mut spawn_ms = Vec::with_capacity(iterations);
    for _ in 0..iterations {
        harness.stop().await;
        let started = Instant::now();
        harness.launch().await?;
        let listen = started.elapsed();
        let info = format!("{}/api/info", harness.base_url());
        wait_info(&client, &info, INFO_TIMEOUT).await?;
        let ready = started.elapsed();
        listen_ms.push(millis(ready) - millis(listen));
        spawn_ms.push(millis(ready));
    }
    harness.stop().await;

    // Cold: a fresh temp tree per sample; includes migrations and DB creation.
    let cold = {
        let mut fresh = ServerHarness::prepare(binary, config)?;
        let started = Instant::now();
        fresh.launch().await?;
        let listen = started.elapsed();
        let info = format!("{}/api/info", fresh.base_url());
        wait_info(&client, &info, INFO_TIMEOUT).await?;
        let ready = started.elapsed();
        fresh.stop().await;
        (millis(ready), millis(ready) - millis(listen))
    };

    let warm_p50 = percentile(&mut listen_ms, 50.0);
    let warm_p95 = percentile(&mut listen_ms, 95.0);
    let spawn_p95 = percentile(&mut spawn_ms, 95.0);
    Ok(vec![
        Metric::gate(
            "startup.warm.listen_p95",
            warm_p95,
            "ms",
            BUDGET_STARTUP_WARM_MS,
            "ms",
            "docs/15-performance.md:11",
            format!(
                "{iterations} warm restarts of one temp data dir; listen→info p50 {warm_p50:.2} ms; spawn→info p95 {spawn_p95:.2} ms"
            ),
        ),
        Metric::gate(
            "startup.cold.spawn_to_info",
            cold.0,
            "ms",
            BUDGET_STARTUP_COLD_MS,
            "ms",
            "docs/15-performance.md:12",
            format!(
                "fresh data dir, first run; listen→info {:.2} ms of it (gated value is stricly stricter: it includes process exec and migrations)",
                cold.1
            ),
        ),
    ])
}

// --- idle RSS --------------------------------------------------------------

async fn measure_idle_rss(
    binary: Option<&Path>,
    pid: Option<u32>,
    idle_seconds: u64,
) -> anyhow::Result<Vec<Metric>> {
    if !cfg!(target_os = "linux") {
        return Ok(vec![Metric::skip(
            "idle_rss.vmrss",
            BUDGET_IDLE_RSS_MB,
            "MB",
            "docs/15-performance.md:13",
            "VmRSS sampling reads /proc/<pid>/status; only implemented on Linux",
        )]);
    }

    let (pid, harness) = if let Some(pid) = pid {
        (pid, None)
    } else {
        let binary = binary.context("idle-rss needs a server binary or --pid")?;
        let harness = ServerHarness::start(binary, TestConfig::new()).await?;
        let pid = harness.pid().context("running server has no process id")?;
        (pid, Some(harness))
    };

    tokio::time::sleep(Duration::from_secs(idle_seconds)).await;
    let (rss_mb, peak_mb) = read_vmrss(pid)?;
    if let Some(mut harness) = harness {
        harness.stop().await;
    }

    Ok(vec![Metric::gate(
        "idle_rss.vmrss",
        rss_mb,
        "MB",
        BUDGET_IDLE_RSS_MB,
        "MB",
        "docs/15-performance.md:13",
        format!(
            "pid {pid} after {idle_seconds}s idle, no session, no LSP, no plugins; VmHWM (peak) {peak_mb:.1} MB"
        ),
    )])
}

fn read_vmrss(pid: u32) -> anyhow::Result<(f64, f64)> {
    let path = format!("/proc/{pid}/status");
    let text =
        std::fs::read_to_string(&path).with_context(|| format!("read {path} (pid {pid})"))?;
    let mut rss_kb = None;
    let mut hwm_kb = None;
    for line in text.lines() {
        if let Some(rest) = line.strip_prefix("VmRSS:") {
            rss_kb = parse_kb(rest);
        }
        if let Some(rest) = line.strip_prefix("VmHWM:") {
            hwm_kb = parse_kb(rest);
        }
    }
    let rss_kb = rss_kb.with_context(|| format!("{path} has no VmRSS line"))?;
    Ok((rss_kb / 1024.0, hwm_kb.unwrap_or(rss_kb) / 1024.0))
}

fn parse_kb(raw: &str) -> Option<f64> {
    raw.split_whitespace().next()?.parse::<f64>().ok()
}

// --- history ---------------------------------------------------------------

async fn measure_history(
    binary: &Path,
    messages: usize,
    iterations: usize,
) -> anyhow::Result<Vec<Metric>> {
    let client = build_client()?;
    let config = TestConfig::new().with_env("OPENCODE_SERVER_PASSWORD", PASSWORD);
    let mut harness = ServerHarness::prepare(binary, config)?;
    seed_history(harness.db_path(), harness.directory(), messages).await?;
    harness.launch().await?;
    let base = harness.base_url().to_owned();
    wait_info(&client, &format!("{base}/api/info"), INFO_TIMEOUT).await?;

    let first_page = format!("{base}/api/session/{SESSION_ID}/message?limit=50&order=asc");
    // Warm the page path once so the sample is not the first SQLite read.
    let (status, _) = get_json(&client, &first_page).await?;
    if !status {
        bail!("first history page request failed");
    }

    let walk = walk_history(&client, &base).await?;
    let mut page_ms = Vec::with_capacity(iterations);
    for _ in 0..iterations {
        let started = Instant::now();
        let (ok, _) = get_json(&client, &first_page).await?;
        let elapsed = started.elapsed();
        if !ok {
            bail!("history page request failed while sampling");
        }
        page_ms.push(millis(elapsed));
    }
    harness.stop().await;

    let page_p50 = percentile(&mut page_ms, 50.0);
    let page_p95 = percentile(&mut page_ms, 95.0);
    let walk_p95 = percentile(&mut walk.timings.clone(), 95.0);
    let walk_mean = walk.timings.iter().sum::<f64>() / len_as_f64(walk.timings.len()).max(1.0);
    Ok(vec![
        Metric::gate(
            "history.page.p95",
            page_p95,
            "ms",
            BUDGET_HISTORY_MS,
            "ms",
            "docs/15-performance.md:25",
            format!(
                "{messages}-message transcript, 50-row page over HTTP; {iterations} samples, p50 {page_p50:.2} ms"
            ),
        ),
        Metric::gate(
            "history.walk.p95",
            walk_p95,
            "ms",
            BUDGET_HISTORY_MS,
            "ms",
            "docs/15-performance.md:25",
            format!(
                "{} pages of 50 rows walked with the `next` cursor; mean {walk_mean:.2} ms per page",
                walk.pages
            ),
        ),
    ])
}

struct Walk {
    pages: usize,
    timings: Vec<f64>,
}

async fn walk_history(client: &Client, base: &str) -> anyhow::Result<Walk> {
    let mut timings = Vec::new();
    let mut pages = 0usize;
    let mut cursor: Option<String> = None;
    loop {
        let url = match &cursor {
            Some(cursor) => format!(
                "{base}/api/session/{SESSION_ID}/message?limit=50&order=asc&cursor={cursor}"
            ),
            None => format!("{base}/api/session/{SESSION_ID}/message?limit=50&order=asc"),
        };
        let started = Instant::now();
        let (ok, body) = get_json(client, &url).await?;
        timings.push(millis(started.elapsed()));
        if !ok {
            bail!("history page request failed: {body}");
        }
        pages += 1;
        let items = body
            .get("data")
            .and_then(Value::as_array)
            .map_or(0, Vec::len);
        if items == 0 {
            break;
        }
        cursor = body
            .pointer("/cursor/next")
            .and_then(Value::as_str)
            .map(str::to_owned);
        if cursor.is_none() {
            break;
        }
        if pages > 10_000 {
            bail!("history pagination did not terminate after {pages} pages");
        }
    }
    Ok(Walk { pages, timings })
}

async fn seed_history(db: &Path, directory: &Path, messages: usize) -> anyhow::Result<()> {
    let started = Instant::now();
    {
        let store = Store::open(db)
            .await
            .with_context(|| format!("open bench database {}", db.display()))?;
        let created = now_ms();
        store
            .create_session(NewSession {
                id: SESSION_ID.to_owned(),
                parent_id: None,
                project_id: PROJECT_ID.to_owned(),
                directory: directory.to_string_lossy().into_owned(),
                title: Some("oc-bench history".to_owned()),
                agent: None,
                model: None,
                metadata: None,
                permissions: None,
                time_created: created,
            })
            .await
            .context("create bench session")?;
        for index in 0..messages {
            let kind = if index % 2 == 0 { "user" } else { "assistant" };
            let text = format!(
                "bench message {index} {}",
                "lorem ipsum dolor sit amet ".repeat(8)
            );
            let body = json!({
                "role": kind,
                "content": [{"type": "text", "text": text}],
            });
            store
                .append_message(NewMessage {
                    id: format!("msg_ocbench{index:08}"),
                    session_id: SESSION_ID.to_owned(),
                    kind: kind.to_owned(),
                    data: body,
                    time_created: created + i64::try_from(index).unwrap_or_default(),
                })
                .await
                .with_context(|| format!("append bench message {index}"))?;
        }
    }
    println!(
        "oc-bench: seeded {messages} messages into {} in {:.1}s",
        db.display(),
        started.elapsed().as_secs_f64()
    );
    Ok(())
}

// --- HTTP ------------------------------------------------------------------

fn build_client() -> anyhow::Result<Client> {
    Client::builder()
        .no_proxy()
        .timeout(Duration::from_secs(30))
        .build()
        .context("build bench HTTP client")
}

fn authorization() -> String {
    format!("Basic {}", BASE64.encode(format!("opencode:{PASSWORD}")))
}

async fn wait_info(client: &Client, url: &str, timeout: Duration) -> anyhow::Result<()> {
    let deadline = Instant::now() + timeout;
    loop {
        if let Ok(response) = client
            .get(url)
            .header(reqwest::header::AUTHORIZATION, authorization())
            .send()
            .await
            && response.status().is_success()
        {
            return Ok(());
        }
        if Instant::now() >= deadline {
            bail!("{url} did not answer 200 within {timeout:?}");
        }
        tokio::time::sleep(Duration::from_millis(1)).await;
    }
}

async fn get_json(client: &Client, url: &str) -> anyhow::Result<(bool, Value)> {
    let response = client
        .get(url)
        .header(reqwest::header::AUTHORIZATION, authorization())
        .send()
        .await
        .with_context(|| format!("GET {url}"))?;
    let status = response.status();
    let body: Value = response
        .json()
        .await
        .with_context(|| format!("decode JSON from {url} (status {status})"))?;
    Ok((status.is_success(), body))
}

// --- binary discovery ------------------------------------------------------

fn resolve_binary(explicit: Option<&Path>) -> anyhow::Result<PathBuf> {
    if let Some(explicit) = explicit {
        if explicit.is_file() {
            return Ok(explicit.to_path_buf());
        }
        bail!("--binary {} is not a file", explicit.display());
    }
    let name = format!("opencode{}", std::env::consts::EXE_SUFFIX);
    let mut candidates: Vec<PathBuf> = Vec::new();
    if let Some(env) = std::env::var_os("OC_BENCH_BIN") {
        candidates.push(PathBuf::from(env));
    }
    if let Ok(exe) = std::env::current_exe()
        && let Some(dir) = exe.parent()
    {
        candidates.push(dir.join(&name));
    }
    if let Some(target) = std::env::var_os("CARGO_TARGET_DIR") {
        let target = PathBuf::from(target);
        candidates.push(target.join("release").join(&name));
        candidates.push(target.join("debug").join(&name));
    }
    candidates.push(PathBuf::from("target/release").join(&name));
    candidates.push(PathBuf::from("target/debug").join(&name));
    for candidate in &candidates {
        if candidate.is_file() {
            return Ok(candidate.clone());
        }
    }
    let tried = candidates
        .iter()
        .map(|path| path.display().to_string())
        .collect::<Vec<_>>()
        .join(", ");
    bail!("could not find the `{name}` server binary; pass --binary <path> (tried: {tried})")
}

// --- helpers ---------------------------------------------------------------

fn millis(duration: Duration) -> f64 {
    duration.as_secs_f64() * 1000.0
}

#[allow(clippy::cast_precision_loss)]
fn len_as_f64(len: usize) -> f64 {
    len as f64
}

#[allow(
    clippy::cast_precision_loss,
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss
)]
fn percentile(values: &mut [f64], percent: f64) -> f64 {
    if values.is_empty() {
        return f64::NAN;
    }
    values.sort_by(f64::total_cmp);
    let rank = (percent / 100.0 * values.len() as f64).ceil() as usize;
    values[rank.saturating_sub(1).min(values.len() - 1)]
}

fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |duration| {
            i64::try_from(duration.as_millis()).unwrap_or(i64::MAX)
        })
}

// --- report ----------------------------------------------------------------

fn print_report(metrics: &[Metric]) {
    println!();
    println!(
        "{:<28} {:>12} {:>16} {:>24}  status",
        "metric", "measured", "budget", "source"
    );
    for metric in metrics {
        let measured = if metric.status == Status::Skip {
            "n/a".to_owned()
        } else {
            format!("{:.2} {}", metric.value, metric.unit)
        };
        let budget = format!("<= {:.0} {}", metric.budget, metric.budget_unit);
        println!(
            "{:<28} {:>12} {:>16} {:>24}  {}",
            metric.name,
            measured,
            budget,
            metric.source,
            metric.status.label()
        );
    }
    println!();
    for metric in metrics {
        if !metric.detail.is_empty() {
            println!("{:<28} {}", metric.name, metric.detail);
        }
    }
    let unmet = metrics
        .iter()
        .filter(|metric| metric.status == Status::Unmet)
        .count();
    if unmet == 0 {
        println!("\noc-bench: all measured gates pass");
    } else {
        println!(
            "\noc-bench: {unmet} measured gate(s) UNMET — the budgets are unchanged; the implementation has to catch up"
        );
    }
}

// --- args ------------------------------------------------------------------

const USAGE: &str = "\
oc-bench — performance gates from docs/15-performance.md

usage: oc-bench <startup|idle-rss|history|all> [options]

  --pid <PID>            measure an already-running server (idle-rss only)
  --binary <PATH>        server binary; default: sibling of this executable.
                         A relative path is resolved against the current
                         directory now, because the server is spawned inside
                         its own temp working directory
  --iterations <N>       warm startup/page samples (default 20)
  --idle-seconds <N>     idle time before the RSS sample (default 60)
  --messages <N>         transcript size for `history` (default 5000)
  -h, --help             print this help";

fn parse_args(mut raw: impl Iterator<Item = String>) -> anyhow::Result<Args> {
    let mut args = Args::default();
    let mut command = None;
    while let Some(arg) = raw.next() {
        match arg.as_str() {
            "startup" => command = Some(Command::Startup),
            "idle-rss" | "rss" => command = Some(Command::IdleRss),
            "history" => command = Some(Command::History),
            "all" => command = Some(Command::All),
            "-h" | "--help" => command = Some(Command::Help),
            "--pid" => args.pid = Some(next_value(&mut raw, "--pid")?.parse()?),
            "--binary" | "-b" => {
                args.binary = Some(absolute(&next_value(&mut raw, "--binary")?)?);
            }
            "--iterations" => args.iterations = next_value(&mut raw, "--iterations")?.parse()?,
            "--idle-seconds" => {
                args.idle_seconds = next_value(&mut raw, "--idle-seconds")?.parse()?;
            }
            "--messages" => args.messages = next_value(&mut raw, "--messages")?.parse()?,
            other => bail!("unknown argument {other:?}"),
        }
    }
    args.command =
        command.context("missing command; expected startup, idle-rss, history or all")?;
    Ok(args)
}

fn next_value(raw: &mut impl Iterator<Item = String>, flag: &str) -> anyhow::Result<String> {
    raw.next().with_context(|| format!("{flag} needs a value"))
}

/// Resolve `path` against the current directory.
///
/// The server is spawned with its temp tree as the working directory, so a
/// relative `--binary` would otherwise be resolved there and fail with ENOENT.
fn absolute(path: &str) -> anyhow::Result<PathBuf> {
    std::path::absolute(path).with_context(|| format!("resolve --binary {path}"))
}

#[cfg(test)]
mod tests {
    use super::{absolute, parse_args, parse_kb, percentile};

    #[test]
    fn binary_argument_is_absolutized() {
        let args = parse_args(
            ["idle-rss", "--binary", "target/release/opencode"]
                .into_iter()
                .map(str::to_owned),
        )
        .expect("parse idle-rss with relative --binary");
        let binary = args.binary.expect("binary is set");
        assert!(binary.is_absolute());
        assert!(binary.ends_with("target/release/opencode"));
        assert!(absolute("relative/path").expect("resolve").is_absolute());
    }

    #[test]
    fn percentile_uses_nearest_rank() {
        let mut values = vec![5.0, 1.0, 3.0, 2.0, 4.0];
        assert!((percentile(&mut values, 50.0) - 3.0).abs() < f64::EPSILON);
        assert!((percentile(&mut values, 95.0) - 5.0).abs() < f64::EPSILON);
        assert!((percentile(&mut values, 100.0) - 5.0).abs() < f64::EPSILON);
    }

    #[test]
    fn parses_proc_status_kibibytes() {
        assert_eq!(parse_kb("  1234 kB\n"), Some(1234.0));
        assert_eq!(parse_kb(""), None);
    }
}
