//! Post-edit graph radar (`situational_awareness` / `quality_delta` / native
//! affected tests) and graph-partitioned task slicing (`explore(partition=N)`).

use std::collections::{BTreeSet, VecDeque};
use std::fmt::Write as _;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::{Mutex as StdMutex, OnceLock};
use std::time::{Duration, Instant};

use serde_json::{Value, json};
use tokio_util::sync::CancellationToken;

use super::graph_ops::{extract_xml_attr, parse_mcp_json_text};
use super::native_index::{
    NativePartitionSlice, native_affected_tests_for_files, native_partition_task, tree_root_for,
};
use super::precision::{PrecisionMode, scip_for_mode};
use super::ripwire::{compact_ripwire_cli_output, find_ripwire_binary};
use super::ripwire_mcp::call_mcp_tool_with_scip;
use crate::{ToolContext, ToolError};

/// TTL for cached partitioned task bundles so parallel manual `subagent` calls
/// share the exact same graph partition without re-running `explore`.
const PARTITION_CACHE_TTL: Duration = Duration::from_secs(60);
const MAX_PARTITION_CACHE_ENTRIES: usize = 8;

/// Fast timeout for non-blocking post-edit radar queries.
pub const DEFAULT_EDIT_RADAR_TIMEOUT_MS: u64 = 2_500;

/// One disjoint worker slice produced by [`partition_task`].
#[derive(Debug, Clone)]
pub struct PartitionSlice {
    pub index: usize,
    pub files: Vec<String>,
    pub symbols: Vec<String>,
    pub text: String,
}

/// Complete partitioned task graph: shared interface core + N disjoint worker slices.
#[derive(Debug, Clone)]
pub struct PartitionedTask {
    pub shared_core: String,
    pub slices: Vec<PartitionSlice>,
    pub raw_text: String,
    pub engine: String,
}

impl PartitionedTask {
    /// Human+model formatted output when `code_map(operation="pack_task", partitions=N)` runs.
    #[must_use]
    pub fn format_for_tool(&self) -> String {
        if !self.raw_text.is_empty() && self.engine == "ripwire" {
            return self.raw_text.clone();
        }
        let mut out = format!(
            "<ctx-partitions engine=\"{}\" partitions=\"{}\">\n{}\n",
            self.engine,
            self.slices.len(),
            self.shared_core
        );
        for slice in &self.slices {
            let _ = writeln!(out, "{}", slice.text);
        }
        out.push_str("</ctx-partitions>");
        out
    }

    /// Format the prompt supplement injected into subagent worker `index`.
    #[must_use]
    pub fn prompt_section_for(&self, index: usize) -> Option<String> {
        if self.slices.is_empty() {
            return None;
        }
        let slice = &self.slices[index % self.slices.len()];
        let files_attr = if slice.files.is_empty() {
            "none".to_string()
        } else {
            slice.files.join(",")
        };
        let mut out = format!(
            "<partition index=\"{index}\" total=\"{}\" assigned_files=\"{files_attr}\">\n\
             Focus primarily on the modules/symbols assigned below; rely on <shared_core> for \
             cross-module contracts so sibling workers avoid redundant exploration.\n{}\n</partition>",
            self.slices.len(),
            slice.text
        );
        if !self.shared_core.trim().is_empty() {
            let _ = write!(out, "\n<shared_core>\n{}\n</shared_core>", self.shared_core);
        }
        Some(out)
    }

    /// Metadata summary attached to `code_map` or `subagent` results.
    #[must_use]
    pub fn metadata_summary(&self) -> Value {
        let partitions: Vec<Value> = self
            .slices
            .iter()
            .map(|s| {
                json!({
                    "index": s.index,
                    "files": s.files,
                    "symbols": s.symbols,
                })
            })
            .collect();
        json!({
            "engine": self.engine,
            "partition_count": self.slices.len(),
            "partitions": partitions,
        })
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct PartitionCacheKey {
    root: PathBuf,
    task: String,
    partitions: usize,
}

#[derive(Debug)]
struct CachedPartitionEntry {
    key: PartitionCacheKey,
    created: Instant,
    value: PartitionedTask,
}

fn partition_cache() -> &'static StdMutex<VecDeque<CachedPartitionEntry>> {
    static CACHE: OnceLock<StdMutex<VecDeque<CachedPartitionEntry>>> = OnceLock::new();
    CACHE.get_or_init(|| StdMutex::new(VecDeque::new()))
}

fn get_cached_partition(key: &PartitionCacheKey) -> Option<PartitionedTask> {
    let now = Instant::now();
    let mut guard = partition_cache()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    guard.retain(|e| now.duration_since(e.created) < PARTITION_CACHE_TTL);
    guard
        .iter()
        .find(|e| &e.key == key)
        .map(|e| e.value.clone())
}

fn put_cached_partition(key: PartitionCacheKey, value: PartitionedTask) {
    let now = Instant::now();
    let mut guard = partition_cache()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    guard.retain(|e| now.duration_since(e.created) < PARTITION_CACHE_TTL && e.key != key);
    while guard.len() >= MAX_PARTITION_CACHE_ENTRIES {
        guard.pop_front();
    }
    guard.push_back(CachedPartitionEntry {
        key,
        created: now,
        value,
    });
}

/// Partition `task` into `partitions` disjoint graph slices + shared core.
///
/// Uses `ripwire --mcp` (`explore` with `partition: N`) when available, caching
/// results for a short TTL so parallel spawns stay consistent, and falls back
/// cleanly to [`native_partition_task`].
pub async fn partition_task(
    ctx: &ToolContext,
    target_path: &Path,
    task: &str,
    partitions: usize,
    budget_tokens: usize,
) -> Result<PartitionedTask, ToolError> {
    let count = partitions.clamp(2, 16);
    let root = tree_root_for(ctx, target_path);
    let canonical = fs::canonicalize(&root).unwrap_or_else(|_| root.clone());
    let key = PartitionCacheKey {
        root: canonical,
        task: task.trim().to_string(),
        partitions: count,
    };
    if let Some(cached) = get_cached_partition(&key) {
        return Ok(cached);
    }

    if let Some(binary) = find_ripwire_binary() {
        let cancel = ctx.cancel.clone().unwrap_or_default();
        let scip = scip_for_mode(&root, PrecisionMode::Hybrid);
        let timeout = super::ripwire::configured_ripwire_timeout();
        if let Ok(out) = call_mcp_tool_with_scip(
            &binary,
            &root,
            scip.as_deref(),
            &cancel,
            timeout,
            "explore",
            &json!({
                "path": root,
                "task": task,
                "partition": count,
                "budget_tokens": budget_tokens.max(300),
            }),
        )
        .await
        {
            let cleaned = compact_ripwire_cli_output(&out.text);
            if let Some(parsed) = parse_ripwire_partitions(&cleaned, count) {
                put_cached_partition(key, parsed.clone());
                return Ok(parsed);
            }
        }
    }

    let (shared_core, native_slices, engine) =
        native_partition_task(ctx, target_path, task, count, budget_tokens)?;
    let slices: Vec<PartitionSlice> = native_slices.into_iter().map(from_native_slice).collect();
    let result = PartitionedTask {
        shared_core,
        slices,
        raw_text: String::new(),
        engine,
    };
    put_cached_partition(key, result.clone());
    Ok(result)
}

fn from_native_slice(s: NativePartitionSlice) -> PartitionSlice {
    PartitionSlice {
        index: s.index,
        files: s.files,
        symbols: s.symbols,
        text: s.text,
    }
}

fn parse_ripwire_partitions(xml: &str, requested: usize) -> Option<PartitionedTask> {
    let mut shared_core = String::new();
    let mut slices = Vec::new();
    let mut cursor = 0usize;

    while let Some(rel_start) = xml[cursor..].find("<bundle ") {
        let start = cursor + rel_start;
        let Some(rel_tag_end) = xml[start..].find('>') else {
            break;
        };
        let open_tag = &xml[start..=(start + rel_tag_end)];
        let after_open = start + rel_tag_end + 1;
        let Some(rel_close) = xml[after_open..].find("</bundle>") else {
            break;
        };
        let inner = &xml[after_open..after_open + rel_close];
        cursor = after_open + rel_close + "</bundle>".len();

        let role = extract_xml_attr(open_tag, "role").unwrap_or("");
        if role == "core" {
            shared_core = inner.trim().to_string();
        } else if role == "partition" {
            let idx = extract_xml_attr(open_tag, "i")
                .and_then(|v| v.parse::<usize>().ok())
                .unwrap_or(slices.len());
            let (files, symbols) = extract_bundle_files_and_symbols(inner);
            slices.push(PartitionSlice {
                index: idx,
                files,
                symbols,
                text: inner.trim().to_string(),
            });
        }
    }

    if slices.is_empty() {
        return None;
    }
    slices.sort_by_key(|s| s.index);
    if slices.len() > requested {
        slices.truncate(requested);
    }
    Some(PartitionedTask {
        shared_core,
        slices,
        raw_text: xml.to_string(),
        engine: "ripwire".to_string(),
    })
}

fn extract_bundle_files_and_symbols(inner: &str) -> (Vec<String>, Vec<String>) {
    let mut files = BTreeSet::new();
    let mut symbols = Vec::new();
    let mut seen_syms = BTreeSet::new();
    for tag_name in ["<d ", "<f ", "<c "] {
        let mut rest = inner;
        while let Some(pos) = rest.find(tag_name) {
            rest = &rest[pos..];
            let Some(end) = rest.find('>') else {
                break;
            };
            let tag = &rest[..=end];
            rest = &rest[end + 1..];
            if let Some(p) = extract_xml_attr(tag, "p") {
                let file = p.rsplit_once(':').map_or(p, |(f, _)| f);
                if !file.is_empty() {
                    files.insert(file.to_string());
                }
            }
            if let Some(n) = extract_xml_attr(tag, "n")
                && seen_syms.insert(n.to_string())
            {
                symbols.push(n.to_string());
            }
        }
    }
    (files.into_iter().collect(), symbols)
}

/// Structured post-edit / pre-verification graph radar findings.
#[derive(Debug, Clone, Default)]
pub struct EditRadarReport {
    pub suggested_tests: Vec<String>,
    pub forgotten_partners: Vec<String>,
    pub contract_warnings: Vec<String>,
    pub blast_radius_files: Vec<String>,
    pub regressions: Vec<String>,
}

impl EditRadarReport {
    #[must_use]
    pub fn is_actionable(&self) -> bool {
        !self.suggested_tests.is_empty()
            || !self.forgotten_partners.is_empty()
            || !self.contract_warnings.is_empty()
            || !self.regressions.is_empty()
    }

    /// Render a compact XML/text supplement when actionable signals exist.
    #[must_use]
    pub fn render_text(&self) -> Option<String> {
        if !self.is_actionable() {
            return None;
        }
        let mut out = String::from("<post_edit_radar>\n");
        if !self.contract_warnings.is_empty() {
            let _ = writeln!(
                out,
                "contract_warnings: {}",
                self.contract_warnings.join("; ")
            );
        }
        if !self.forgotten_partners.is_empty() {
            let _ = writeln!(
                out,
                "forgotten_cochange_partners: {}",
                self.forgotten_partners.join(", ")
            );
        }
        if !self.suggested_tests.is_empty() {
            let _ = writeln!(out, "tests_to_run: {}", self.suggested_tests.join(", "));
        }
        if !self.regressions.is_empty() {
            let _ = writeln!(out, "quality_delta: {}", self.regressions.join("; "));
        }
        if !self.blast_radius_files.is_empty() {
            let _ = writeln!(
                out,
                "top_blast_radius: {}",
                self.blast_radius_files.join(", ")
            );
        }
        out.push_str("</post_edit_radar>");
        Some(out)
    }

    #[must_use]
    pub fn to_metadata(&self) -> Option<Value> {
        if !self.is_actionable() && self.blast_radius_files.is_empty() {
            return None;
        }
        Some(json!({
            "tests_to_run": self.suggested_tests,
            "forgotten_partners": self.forgotten_partners,
            "contract_warnings": self.contract_warnings,
            "blast_radius_files": self.blast_radius_files,
            "regressions": self.regressions,
        }))
    }
}

/// Collect post-edit graph intelligence (impacted tests, forgotten co-change
/// partners, blast radius) for `changed_files` using warm `ripwire --mcp` when
/// available or cached pure-Rust fallback.
pub async fn collect_post_edit_radar(
    root: &Path,
    changed_files: &[String],
    include_quality_delta: bool,
) -> EditRadarReport {
    let mut report = EditRadarReport::default();
    if changed_files.is_empty() {
        return report;
    }

    let timeout = Duration::from_millis(DEFAULT_EDIT_RADAR_TIMEOUT_MS);
    let cancel = CancellationToken::new();
    let mut got_mcp = false;

    if let Some(binary) = find_ripwire_binary() {
        let csv = changed_files.join(",");
        if let Ok(out) = call_mcp_tool_with_scip(
            &binary,
            root,
            None,
            &cancel,
            timeout,
            "situational_awareness",
            &json!({
                "path": root,
                "files": csv,
                "limit": 10,
            }),
        )
        .await
            && let Some(val) = parse_mcp_json_text(&out.text)
        {
            got_mcp = true;
            populate_from_situational_awareness(&val, &mut report);
        }

        if include_quality_delta
            && let Ok(out) = call_mcp_tool_with_scip(
                &binary,
                root,
                None,
                &cancel,
                timeout,
                "quality_delta",
                &json!({ "path": root }),
            )
            .await
            && let Some(val) = parse_mcp_json_text(&out.text)
        {
            populate_from_quality_delta(&val, &mut report);
        }
    }

    if !got_mcp || report.suggested_tests.is_empty() {
        let ctx = ToolContext {
            session_id: "radar".to_string(),
            directory: root.to_path_buf(),
            cwd: root.to_path_buf(),
            cancel: None,
        };
        let native_tests = native_affected_tests_for_files(&ctx, root, changed_files);
        for t in native_tests {
            if !report.suggested_tests.contains(&t) {
                report.suggested_tests.push(t);
            }
        }
    }

    report
}

fn populate_from_situational_awareness(val: &Value, report: &mut EditRadarReport) {
    if let Some(arr) = val.get("tests_to_run").and_then(Value::as_array) {
        for item in arr.iter().take(8) {
            if let Some(file) = extract_path_or_string(item)
                && !report.suggested_tests.contains(&file)
            {
                report.suggested_tests.push(file);
            }
        }
    }
    if let Some(arr) = val.get("forgotten").and_then(Value::as_array) {
        for item in arr.iter().take(6) {
            if let Some(file) = extract_path_or_string(item)
                && !report.forgotten_partners.contains(&file)
            {
                report.forgotten_partners.push(file);
            }
        }
    }
    if let Some(arr) = val.get("blast_radius").and_then(Value::as_array) {
        for item in arr.iter().take(5) {
            if let Some(file) = extract_path_or_string(item)
                && !report.blast_radius_files.contains(&file)
            {
                report.blast_radius_files.push(file);
            }
        }
    }
}

fn populate_from_quality_delta(val: &Value, report: &mut EditRadarReport) {
    let items = val
        .get("r")
        .or_else(|| val.get("regressions"))
        .or_else(|| val.get("findings"))
        .and_then(Value::as_array);
    if let Some(arr) = items {
        for item in arr.iter().take(6) {
            if let Some(s) = item.as_str() {
                let trimmed = s.trim();
                if !trimmed.is_empty() && !report.regressions.contains(&trimmed.to_string()) {
                    report.regressions.push(trimmed.to_string());
                }
                continue;
            }
            let kind = item
                .get("kind")
                .or_else(|| item.get("type"))
                .and_then(Value::as_str)
                .unwrap_or("finding");
            let sym = item
                .get("sym")
                .or_else(|| item.get("symbol"))
                .or_else(|| item.get("name"))
                .and_then(Value::as_str)
                .unwrap_or("");
            let loc = item
                .get("p")
                .or_else(|| item.get("file"))
                .and_then(Value::as_str)
                .unwrap_or("");
            let desc = if !sym.is_empty() && !loc.is_empty() {
                format!("{kind}: `{sym}` ({loc})")
            } else if !loc.is_empty() {
                format!("{kind}: {loc}")
            } else {
                kind.to_string()
            };
            if !report.regressions.contains(&desc) {
                report.regressions.push(desc);
            }
        }
    }
}

fn extract_path_or_string(item: &Value) -> Option<String> {
    if let Some(s) = item.as_str() {
        let trimmed = s.trim();
        return (!trimmed.is_empty()).then(|| trimmed.to_string());
    }
    item.get("file")
        .or_else(|| item.get("path"))
        .or_else(|| item.get("test"))
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(ToString::to_string)
}
