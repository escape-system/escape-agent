//! Cached pure-Rust workspace symbol and call-graph index.
//!
//! Powers every `code_map` operation (`pack_task`, `map`, `communities`,
//! `outline`, `symbol`, `expand`, `impact`, `references`, `connect`,
//! `affected`, `trace`, `batch`, `lego`, `slice`, `cochange`, `doc_drift`,
//! `flags`, `quality`) and `symbol_edit` native fallback when `ripwire` is
//! absent or falls back, validating file `(len, modified)` stamps incrementally
//! so unchanged files are never re-parsed.

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet, VecDeque};
use std::fmt::Write as _;
use std::fs;
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::{Arc, Mutex as StdMutex, OnceLock};
use std::time::{Instant, UNIX_EPOCH};

use super::files::{collect_code_files, read_bounded_file, relative_to};
use super::parsers::parse_symbols;
use super::ripwire::strip_comments_and_blank_runs;
use super::{Symbol, SymbolKind, Visibility};
use crate::{ToolContext, ToolError};

/// Maximum number of workspaces cached in the native graph index pool.
const MAX_CACHED_ROOTS: usize = 2;

/// Maximum lines captured when inferring a single symbol body end line.
const MAX_SYMBOL_SPAN_LINES: usize = 300;

/// Common language keywords ignored when building native call-graph edges.
const KEYWORDS: &[&str] = &[
    "fn",
    "pub",
    "let",
    "mut",
    "const",
    "static",
    "struct",
    "enum",
    "trait",
    "impl",
    "for",
    "loop",
    "while",
    "if",
    "else",
    "match",
    "return",
    "self",
    "Self",
    "super",
    "crate",
    "mod",
    "use",
    "as",
    "in",
    "async",
    "await",
    "move",
    "type",
    "where",
    "true",
    "false",
    "None",
    "Some",
    "Ok",
    "Err",
    "String",
    "Vec",
    "Option",
    "Result",
    "bool",
    "usize",
    "isize",
    "u8",
    "u16",
    "u32",
    "u64",
    "i32",
    "i64",
    "f32",
    "f64",
    "str",
    "def",
    "class",
    "import",
    "from",
    "function",
    "export",
    "default",
    "interface",
    "const",
    "var",
    "new",
    "this",
    "null",
    "undefined",
    "void",
    "func",
    "package",
    "defer",
    "go",
];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct FileStamp {
    len: u64,
    modified_nanos: Option<u128>,
}

impl FileStamp {
    fn of(path: &Path) -> Option<Self> {
        let meta = fs::metadata(path).ok()?;
        let modified_nanos = meta
            .modified()
            .ok()
            .and_then(|ts| ts.duration_since(UNIX_EPOCH).ok())
            .map(|d| d.as_nanos());
        Some(Self {
            len: meta.len(),
            modified_nanos,
        })
    }
}

#[derive(Debug, Clone)]
pub(super) struct IndexedSymbol {
    pub(super) id: usize,
    pub(super) file_idx: usize,
    pub(super) rel_path: String,
    pub(super) sym: Symbol,
    pub(super) start_line: usize,
    pub(super) end_line: usize,
}

#[derive(Debug, Clone)]
struct IndexedFile {
    path: PathBuf,
    rel_path: String,
    ext: String,
    stamp: FileStamp,
    lines: Vec<String>,
    symbols: Vec<IndexedSymbol>,
    referenced_idents: Vec<Vec<String>>,
}

#[derive(Debug)]
struct CachedWorkspace {
    last_used: Instant,
    files_by_path: HashMap<PathBuf, IndexedFile>,
    file_order: Vec<PathBuf>,
    all_symbols: Vec<IndexedSymbol>,
    name_to_ids: HashMap<String, Vec<usize>>,
    callees: Vec<Vec<usize>>,
    callers: Vec<Vec<usize>>,
}

impl CachedWorkspace {
    fn empty() -> Self {
        Self {
            last_used: Instant::now(),
            files_by_path: HashMap::new(),
            file_order: Vec::new(),
            all_symbols: Vec::new(),
            name_to_ids: HashMap::new(),
            callees: Vec::new(),
            callers: Vec::new(),
        }
    }

    fn refresh(&mut self, ctx: &ToolContext, root: &Path) -> Result<(), ToolError> {
        self.last_used = Instant::now();
        let disk_files = collect_code_files(ctx, root)?;
        let disk_set: HashSet<PathBuf> = disk_files.iter().cloned().collect();

        let prev_len = self.files_by_path.len();
        self.files_by_path.retain(|p, _| disk_set.contains(p));
        let mut dirty = self.files_by_path.len() != prev_len;

        for path in &disk_files {
            let Some(stamp) = FileStamp::of(path) else {
                continue;
            };
            let up_to_date = self
                .files_by_path
                .get(path)
                .is_some_and(|existing| existing.stamp == stamp);
            if up_to_date {
                continue;
            }
            if let Ok(indexed) = build_indexed_file(root, path, stamp) {
                self.files_by_path.insert(path.clone(), indexed);
                dirty = true;
            }
        }

        if dirty || self.file_order.len() != self.files_by_path.len() {
            self.rebuild_graph();
        }
        Ok(())
    }

    fn rebuild_graph(&mut self) {
        self.file_order = self.files_by_path.keys().cloned().collect();
        self.file_order.sort();

        self.all_symbols.clear();
        self.name_to_ids.clear();

        let mut per_sym_idents: Vec<(usize, Vec<String>)> = Vec::new();

        for (file_idx, path) in self.file_order.iter().enumerate() {
            let Some(file) = self.files_by_path.get_mut(path) else {
                continue;
            };
            for (local_idx, sym) in file.symbols.iter_mut().enumerate() {
                let global_id = self.all_symbols.len();
                sym.id = global_id;
                sym.file_idx = file_idx;
                self.name_to_ids
                    .entry(sym.sym.name.to_lowercase())
                    .or_default()
                    .push(global_id);
                self.all_symbols.push(sym.clone());
                let idents = file
                    .referenced_idents
                    .get(local_idx)
                    .cloned()
                    .unwrap_or_default();
                per_sym_idents.push((file_idx, idents));
            }
        }

        let n = self.all_symbols.len();
        self.callees = vec![Vec::new(); n];
        self.callers = vec![Vec::new(); n];

        for (caller_id, (caller_file_idx, idents)) in per_sym_idents.into_iter().enumerate() {
            let caller_name = self.all_symbols[caller_id].sym.name.to_lowercase();
            let mut seen_targets = HashSet::new();
            for ident in idents {
                let key = ident.to_lowercase();
                if key == caller_name {
                    continue;
                }
                let Some(candidates) = self.name_to_ids.get(&key) else {
                    continue;
                };
                // Prefer same-file definitions when overloaded; otherwise link up to 3 targets.
                let mut same_file: Vec<usize> = candidates
                    .iter()
                    .copied()
                    .filter(|&cid| self.all_symbols[cid].file_idx == caller_file_idx)
                    .collect();
                if same_file.is_empty() {
                    same_file.extend(candidates.iter().copied().take(3));
                }
                for callee_id in same_file {
                    if callee_id != caller_id && seen_targets.insert(callee_id) {
                        self.callees[caller_id].push(callee_id);
                        self.callers[callee_id].push(caller_id);
                    }
                }
            }
        }
    }

    fn fan_in(&self, id: usize) -> usize {
        self.callers.get(id).map_or(0, Vec::len)
    }

    fn fan_out(&self, id: usize) -> usize {
        self.callees.get(id).map_or(0, Vec::len)
    }

    fn symbols_in_scope<'a>(&'a self, root: &Path, target_path: &Path) -> Vec<&'a IndexedSymbol> {
        let rel_scope = relative_scope(root, target_path);
        self.all_symbols
            .iter()
            .filter(|s| path_matches_scope(&s.rel_path, rel_scope.as_deref()))
            .collect()
    }

    fn find_matching_symbols(&self, target_scope: Option<&str>, raw_query: &str) -> Vec<usize> {
        let q = raw_query.trim();
        if q.is_empty() {
            return Vec::new();
        }

        // Handle @FILE:LINE or file:symbol syntax.
        if let Some(rest) = q.strip_prefix('@')
            && let Some((file_part, line_str)) = rest.rsplit_once(':')
            && let Ok(line_num) = line_str.trim().parse::<usize>()
        {
            return self.find_by_file_line(file_part.trim(), line_num);
        }

        if let Some((file_part, sym_part)) = q.split_once(':')
            && !file_part.contains(' ')
            && !sym_part.is_empty()
        {
            if let Ok(line_num) = sym_part.trim().parse::<usize>() {
                let by_line = self.find_by_file_line(file_part.trim(), line_num);
                if !by_line.is_empty() {
                    return by_line;
                }
            }
            let sym_lower = sym_part.trim().to_lowercase();
            let file_norm = file_part.trim().replace('\\', "/").to_lowercase();
            let mut matched: Vec<usize> = self
                .all_symbols
                .iter()
                .filter(|s| {
                    path_matches_scope(&s.rel_path, target_scope)
                        && s.rel_path.to_lowercase().ends_with(&file_norm)
                        && s.sym.name.to_lowercase() == sym_lower
                })
                .map(|s| s.id)
                .collect();
            if !matched.is_empty() {
                return matched;
            }
            matched = self
                .all_symbols
                .iter()
                .filter(|s| {
                    path_matches_scope(&s.rel_path, target_scope)
                        && s.rel_path.to_lowercase().contains(&file_norm)
                        && s.sym.name.to_lowercase().contains(&sym_lower)
                })
                .map(|s| s.id)
                .collect();
            if !matched.is_empty() {
                return matched;
            }
        }

        let needle = q.to_lowercase();
        let mut exact = Vec::new();
        let mut partial = Vec::new();
        for s in &self.all_symbols {
            if !path_matches_scope(&s.rel_path, target_scope) {
                continue;
            }
            let lower = s.sym.name.to_lowercase();
            if lower == needle {
                exact.push(s.id);
            } else if lower.contains(&needle) {
                partial.push(s.id);
            }
        }
        if exact.is_empty() { partial } else { exact }
    }

    fn find_by_file_line(&self, file_part: &str, line_num: usize) -> Vec<usize> {
        let needle = file_part.replace('\\', "/").to_lowercase();
        let mut best: Option<(usize, usize)> = None; // (span_len, id)
        for s in &self.all_symbols {
            let rel_low = s.rel_path.to_lowercase();
            if !(rel_low == needle || rel_low.ends_with(&needle) || rel_low.contains(&needle)) {
                continue;
            }
            if line_num >= s.start_line && line_num <= s.end_line {
                let span = s.end_line.saturating_sub(s.start_line);
                if best.is_none_or(|(best_span, _)| span < best_span) {
                    best = Some((span, s.id));
                }
            }
        }
        if let Some((_, id)) = best {
            return vec![id];
        }
        // Nearest preceding symbol in matching file.
        self.all_symbols
            .iter()
            .filter(|s| {
                let rel_low = s.rel_path.to_lowercase();
                (rel_low == needle || rel_low.ends_with(&needle) || rel_low.contains(&needle))
                    && s.start_line <= line_num
            })
            .max_by_key(|s| s.start_line)
            .map(|s| vec![s.id])
            .unwrap_or_default()
    }
}

fn cache_pool() -> &'static StdMutex<HashMap<PathBuf, Arc<StdMutex<CachedWorkspace>>>> {
    static POOL: OnceLock<StdMutex<HashMap<PathBuf, Arc<StdMutex<CachedWorkspace>>>>> =
        OnceLock::new();
    POOL.get_or_init(|| StdMutex::new(HashMap::new()))
}

fn with_index<T>(
    ctx: &ToolContext,
    root: &Path,
    f: impl FnOnce(&CachedWorkspace) -> Result<T, ToolError>,
) -> Result<T, ToolError> {
    let canonical = fs::canonicalize(root).unwrap_or_else(|_| root.to_path_buf());
    let entry = {
        let mut guard = cache_pool()
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        while guard.len() >= MAX_CACHED_ROOTS && !guard.contains_key(&canonical) {
            let oldest_key = guard
                .iter()
                .min_by_key(|(_, ws)| {
                    ws.lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner)
                        .last_used
                })
                .map(|(k, _)| k.clone());
            if let Some(k) = oldest_key {
                guard.remove(&k);
            } else {
                break;
            }
        }
        Arc::clone(
            guard
                .entry(canonical)
                .or_insert_with(|| Arc::new(StdMutex::new(CachedWorkspace::empty()))),
        )
    };

    let mut ws = entry
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    ws.refresh(ctx, root)?;
    f(&ws)
}

fn build_indexed_file(
    root: &Path,
    path: &Path,
    stamp: FileStamp,
) -> Result<IndexedFile, ToolError> {
    let content = read_bounded_file(path)?;
    let ext = path
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or("")
        .to_string();
    let rel_path = relative_to(root, path).replace('\\', "/");
    let lines: Vec<String> = content.lines().map(ToString::to_string).collect();
    let parsed = parse_symbols(&content, &ext);

    let mut symbols = Vec::with_capacity(parsed.len());
    let mut referenced_idents = Vec::with_capacity(parsed.len());

    for sym in parsed {
        let start_line = sym.line.max(1);
        let next_start = usize::MAX;
        let end_line = compute_symbol_end_line(&lines, &ext, start_line, next_start);
        symbols.push(IndexedSymbol {
            id: 0,
            file_idx: 0,
            rel_path: rel_path.clone(),
            sym,
            start_line,
            end_line,
        });
    }

    // Refine Python/Markdown spans using following sibling start lines.
    if matches!(ext.to_lowercase().as_str(), "md" | "markdown") {
        for idx in 0..symbols.len() {
            let next_line = symbols
                .get(idx + 1)
                .map_or(lines.len().saturating_add(1), |s| s.start_line);
            symbols[idx].end_line = next_line.saturating_sub(1).max(symbols[idx].start_line);
        }
    }

    for sym in &symbols {
        let start_idx = sym.start_line.saturating_sub(1);
        let end_idx = sym.end_line.min(lines.len());
        let mut seen = HashSet::new();
        let mut idents = Vec::new();
        for line in lines.get(start_idx..end_idx).unwrap_or(&[]) {
            for tok in extract_call_idents(line) {
                if seen.insert(tok.clone()) {
                    idents.push(tok);
                }
            }
        }
        referenced_idents.push(idents);
    }

    Ok(IndexedFile {
        path: path.to_path_buf(),
        rel_path,
        ext,
        stamp,
        lines,
        symbols,
        referenced_idents,
    })
}

fn compute_symbol_end_line(
    lines: &[String],
    ext: &str,
    start_line_1: usize,
    _next_line_1: usize,
) -> usize {
    let total = lines.len();
    if start_line_1 == 0 || start_line_1 > total {
        return start_line_1;
    }
    let start_idx = start_line_1 - 1;
    let limit_idx = (start_idx + MAX_SYMBOL_SPAN_LINES).min(total);
    let ext_low = ext.to_lowercase();

    if ext_low == "py" {
        let base_indent = lines[start_idx]
            .bytes()
            .take_while(|b| *b == b' ' || *b == b'\t')
            .count();
        let mut last_non_blank = start_line_1;
        for (offset, line) in lines[start_idx + 1..limit_idx].iter().enumerate() {
            let trimmed = line.trim();
            if trimmed.is_empty() || trimmed.starts_with('#') {
                continue;
            }
            let indent = line
                .bytes()
                .take_while(|b| *b == b' ' || *b == b'\t')
                .count();
            if indent <= base_indent {
                break;
            }
            last_non_blank = start_line_1 + 1 + offset;
        }
        return last_non_blank;
    }

    // Brace / semicolon scan for C-family, Rust, TS/JS, Go.
    let mut depth: i32 = 0;
    let mut saw_open = false;
    for (offset, line) in lines[start_idx..limit_idx].iter().enumerate() {
        let code = line.split("//").next().unwrap_or(line);
        for ch in code.chars() {
            match ch {
                '{' => {
                    depth += 1;
                    saw_open = true;
                }
                '}' => {
                    depth = depth.saturating_sub(1);
                    if saw_open && depth == 0 {
                        return start_line_1 + offset;
                    }
                }
                ';' if !saw_open && depth == 0 => {
                    return start_line_1 + offset;
                }
                _ => {}
            }
        }
    }
    (start_line_1 + 12).min(total)
}

fn extract_call_idents(line: &str) -> Vec<String> {
    let mut out = Vec::new();
    let bytes = line.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        let b = bytes[i];
        if b.is_ascii_alphabetic() || b == b'_' {
            let start = i;
            i += 1;
            while i < bytes.len() && (bytes[i].is_ascii_alphanumeric() || bytes[i] == b'_') {
                i += 1;
            }
            let ident = &line[start..i];
            if ident.len() >= 3 && !KEYWORDS.contains(&ident) {
                out.push(ident.to_string());
            }
        } else {
            i += 1;
        }
    }
    out
}

pub(super) fn relative_scope(root: &Path, target_path: &Path) -> Option<String> {
    if target_path == root {
        return None;
    }
    let rel = relative_to(root, target_path).replace('\\', "/");
    let trimmed = rel.trim_matches('/').to_string();
    if trimmed.is_empty() || trimmed == "." {
        None
    } else {
        Some(trimmed)
    }
}

fn path_matches_scope(rel_path: &str, scope: Option<&str>) -> bool {
    let Some(prefix) = scope else {
        return true;
    };
    rel_path == prefix || rel_path.starts_with(&format!("{prefix}/"))
}

fn usize_to_f32(n: usize) -> f32 {
    u16::try_from(n).map_or(f32::from(u16::MAX), f32::from)
}

pub(crate) fn tree_root_for(ctx: &ToolContext, target_path: &Path) -> PathBuf {
    if target_path.starts_with(&ctx.directory) {
        ctx.directory.clone()
    } else if target_path.is_file() {
        target_path.parent().unwrap_or(target_path).to_path_buf()
    } else {
        target_path.to_path_buf()
    }
}

/// Cached pure-Rust `outline` for a directory tree using mtime-validated symbols.
pub(super) fn native_outline(
    ctx: &ToolContext,
    target_path: &Path,
    limit: usize,
) -> Result<(String, usize, bool), ToolError> {
    let root = tree_root_for(ctx, target_path);
    with_index(ctx, &root, |ws| {
        let scope = relative_scope(&root, target_path);
        let mut scoped_files = 0usize;
        let mut total_symbols = 0usize;
        let mut shown_symbols = 0usize;
        let mut body = String::new();

        for path in &ws.file_order {
            let Some(file) = ws.files_by_path.get(path) else {
                continue;
            };
            if !path_matches_scope(&file.rel_path, scope.as_deref()) {
                continue;
            }
            scoped_files += 1;
            if file.symbols.is_empty() {
                continue;
            }
            total_symbols += file.symbols.len();
            if shown_symbols >= limit {
                continue;
            }
            let rel = relative_to(&ctx.directory, &file.path);
            let _ = writeln!(body, "\n{rel} ({} symbols):", file.symbols.len());
            for sym in &file.symbols {
                if shown_symbols >= limit {
                    break;
                }
                let _ = writeln!(
                    body,
                    "  L{:<4} [{}] {}",
                    sym.start_line,
                    sym.sym.kind.as_str(),
                    sym.sym.signature
                );
                shown_symbols += 1;
            }
        }

        let mut out = format!(
            "Directory outline for {} ({scoped_files} files):\n{body}",
            target_path.display()
        );
        if shown_symbols < total_symbols {
            let _ = writeln!(
                out,
                "\n[showing {shown_symbols} of {total_symbols} symbols; pass a specific file `path` to narrow down]"
            );
        }
        Ok((out, total_symbols, shown_symbols < total_symbols))
    })
}

type ScoredFileBucket<'a> = (f32, &'a str, Vec<(f32, &'a IndexedSymbol)>);

fn score_map_symbol(
    ws: &CachedWorkspace,
    sym: &IndexedSymbol,
    q_terms: &[String],
    rank_by: Option<&str>,
) -> f32 {
    let name_low = sym.sym.name.to_lowercase();
    let sig_low = sym.sym.signature.to_lowercase();
    let doc_low = sym.sym.doc.as_deref().unwrap_or("").to_lowercase();

    let mut weight = if q_terms.is_empty() {
        match sym.sym.kind {
            SymbolKind::Struct | SymbolKind::Class | SymbolKind::Interface | SymbolKind::Trait => {
                3.0
            }
            SymbolKind::Function | SymbolKind::Method => 2.0,
            _ => 1.0,
        }
    } else {
        let mut total = 0.0_f32;
        for term in q_terms {
            if name_low == *term {
                total += 15.0;
            } else if name_low.contains(term) {
                total += 8.0;
            }
            if sig_low.contains(term) {
                total += 3.0;
            }
            if doc_low.contains(term) {
                total += 2.0;
            }
        }
        total
    };

    if weight == 0.0 {
        return 0.0;
    }

    let fan_in = usize_to_f32(ws.fan_in(sym.id));
    let fan_out = usize_to_f32(ws.fan_out(sym.id));
    let graph_boost = match rank_by {
        Some("coupling") => fan_out * 1.5 + fan_in * 0.5,
        Some("risk") => (fan_in + 1.0) * (fan_out + 1.0) * 0.4,
        _ => fan_in * 1.4 + fan_out * 0.3,
    };
    weight += graph_boost;

    if sym.sym.visibility == Visibility::Public {
        weight *= 1.2;
    }
    weight
}

/// Native `map` operation boosted with call-graph centrality and ranking modes.
pub(super) fn native_map(
    ctx: &ToolContext,
    target_path: &Path,
    query: &str,
    rank_by: Option<&str>,
    max_tokens: usize,
    limit: usize,
) -> Result<(String, usize, usize), ToolError> {
    let root = tree_root_for(ctx, target_path);
    with_index(ctx, &root, |ws| {
        let scope = relative_scope(&root, target_path);
        let q_terms: Vec<String> = query
            .split_whitespace()
            .map(|t| t.trim().to_lowercase())
            .filter(|t| !t.is_empty())
            .collect();

        let mut per_file: HashMap<usize, ScoredFileBucket<'_>> = HashMap::new();
        for sym in &ws.all_symbols {
            if !path_matches_scope(&sym.rel_path, scope.as_deref()) {
                continue;
            }
            let rel_lower = sym.rel_path.to_lowercase();
            let path_bonus: f32 = q_terms
                .iter()
                .filter(|term| rel_lower.contains(term.as_str()))
                .map(|_| 4.0)
                .sum();
            let weight = score_map_symbol(ws, sym, &q_terms, rank_by);
            if weight == 0.0 && path_bonus == 0.0 {
                continue;
            }

            let entry = per_file
                .entry(sym.file_idx)
                .or_insert_with(|| (path_bonus, sym.rel_path.as_str(), Vec::new()));
            entry.2.push((weight, sym));
        }

        let mut ranked_files: Vec<ScoredFileBucket<'_>> = per_file.into_values().collect();
        for (total, _, syms) in &mut ranked_files {
            syms.sort_by(|a, b| b.0.total_cmp(&a.0));
            let top_sum: f32 = syms.iter().take(5).map(|(s, _)| *s).sum();
            *total += top_sum;
        }
        ranked_files.sort_by(|a, b| b.0.total_cmp(&a.0));

        let mut out = format!(
            "<code_map query=\"{query}\" root=\"{}\">\n",
            relative_to(&ctx.directory, target_path)
        );
        let max_chars = max_tokens.saturating_mul(4);
        let mut total_files = 0usize;
        let mut total_symbols = 0usize;

        for (_, rel, syms) in ranked_files.into_iter().take(limit) {
            if out.len() >= max_chars {
                break;
            }
            let _ = writeln!(out, "{rel}:");
            total_files += 1;
            for (_, sym) in syms.into_iter().take(8) {
                if out.len() >= max_chars {
                    break;
                }
                let fan_in = ws.fan_in(sym.id);
                if fan_in > 0 {
                    let _ = writeln!(
                        out,
                        "  L{:<4} {} (callers:{fan_in})",
                        sym.start_line, sym.sym.signature
                    );
                } else {
                    let _ = writeln!(out, "  L{:<4} {}", sym.start_line, sym.sym.signature);
                }
                total_symbols += 1;
            }
        }
        out.push_str("</code_map>\n");
        Ok((out, total_files, total_symbols))
    })
}

/// Native `symbol` lookup enriched with 1-hop callers and callees.
pub(super) fn native_symbol(
    ctx: &ToolContext,
    target_path: &Path,
    query: &str,
    limit: usize,
) -> Result<(String, usize), ToolError> {
    let root = tree_root_for(ctx, target_path);
    with_index(ctx, &root, |ws| {
        let scope = relative_scope(&root, target_path);
        let ids = ws.find_matching_symbols(scope.as_deref(), query);
        let mut lines = Vec::new();
        for &id in ids.iter().take(limit) {
            let s = &ws.all_symbols[id];
            let incoming: Vec<&str> = ws.callers[id]
                .iter()
                .take(4)
                .map(|&cid| ws.all_symbols[cid].sym.name.as_str())
                .collect();
            let outgoing: Vec<&str> = ws.callees[id]
                .iter()
                .take(4)
                .map(|&cid| ws.all_symbols[cid].sym.name.as_str())
                .collect();

            let mut line = format!(
                "{}:{} [{}] {} | handle=@{}:{}",
                s.rel_path,
                s.start_line,
                s.sym.kind.as_str(),
                s.sym.signature,
                s.rel_path,
                s.start_line,
            );
            if !incoming.is_empty() {
                let _ = write!(line, " | callers:[{}]", incoming.join(","));
            }
            if !outgoing.is_empty() {
                let _ = write!(line, " | calls:[{}]", outgoing.join(","));
            }
            lines.push(line);
        }
        let count = lines.len();
        let output = if lines.is_empty() {
            format!("No symbol definitions found matching '{query}'.")
        } else {
            lines.join("\n")
        };
        Ok((output, count))
    })
}

/// Native `expand` operation: extract full or sliced body with optional compression.
pub(super) fn native_expand(
    ctx: &ToolContext,
    target_path: &Path,
    query: &str,
    start_line: Option<usize>,
    end_line: Option<usize>,
    compress: bool,
) -> Result<(String, String), ToolError> {
    let root = tree_root_for(ctx, target_path);
    with_index(ctx, &root, |ws| {
        let scope = relative_scope(&root, target_path);
        let ids = ws.find_matching_symbols(scope.as_deref(), query);
        let Some(&sym_id) = ids.first() else {
            return Err(ToolError::new(format!(
                "No symbol or handle found for expand target '{query}'."
            )));
        };
        let sym = &ws.all_symbols[sym_id];
        let file_path = root.join(&sym.rel_path);
        let file = ws
            .files_by_path
            .get(&file_path)
            .ok_or_else(|| ToolError::new(format!("File not indexed: {}", sym.rel_path)))?;

        let body_start = sym.start_line;
        let refined_end =
            compute_refined_end_line(&file.lines.join("\n"), &file.ext, body_start, sym.end_line);
        let body_end = refined_end.min(file.lines.len());
        let slice_start = start_line
            .map_or(body_start, |off| {
                body_start.saturating_add(off.saturating_sub(1))
            })
            .max(1)
            .min(file.lines.len());
        let slice_end = end_line
            .map_or(body_end, |off| {
                body_start.saturating_add(off.saturating_sub(1))
            })
            .min(file.lines.len())
            .max(slice_start);

        let raw_slice = file.lines[slice_start - 1..slice_end].join("\n");
        let body = if compress {
            strip_comments_and_blank_runs(&raw_slice)
        } else {
            raw_slice
        };

        let out = format!(
            "Symbol: {} [{}] ({}:{}..{})\n```{}\n{}\n```",
            sym.sym.name,
            sym.sym.kind.as_str(),
            sym.rel_path,
            slice_start,
            slice_end,
            file.ext,
            body
        );
        Ok((out, sym.rel_path.clone()))
    })
}

/// Native `impact` operation: BFS reverse-caller blast radius.
pub(super) fn native_impact(
    ctx: &ToolContext,
    target_path: &Path,
    query: &str,
    limit: usize,
) -> Result<(String, usize), ToolError> {
    let root = tree_root_for(ctx, target_path);
    with_index(ctx, &root, |ws| {
        let scope = relative_scope(&root, target_path);
        let seeds = ws.find_matching_symbols(scope.as_deref(), query);
        if seeds.is_empty() {
            return Ok((format!("No symbol found for impact query '{query}'."), 0));
        }

        let mut visited: HashSet<usize> = seeds.iter().copied().collect();
        let mut queue: VecDeque<(usize, usize)> = seeds.iter().map(|&id| (id, 0)).collect();
        let mut impacted: Vec<(usize, &IndexedSymbol)> = Vec::new();

        while let Some((node, depth)) = queue.pop_front() {
            for &caller in &ws.callers[node] {
                if visited.insert(caller) {
                    let next_depth = depth + 1;
                    impacted.push((next_depth, &ws.all_symbols[caller]));
                    if impacted.len() >= limit {
                        break;
                    }
                    queue.push_back((caller, next_depth));
                }
            }
            if impacted.len() >= limit {
                break;
            }
        }

        let seed_desc: Vec<String> = seeds
            .iter()
            .take(3)
            .map(|&id| {
                let s = &ws.all_symbols[id];
                format!("{} ({}:{})", s.sym.name, s.rel_path, s.start_line)
            })
            .collect();
        let mut out = format!(
            "<impact target=\"{query}\" seeds=\"{}\" dependents=\"{}\">\n",
            seed_desc.join(", "),
            impacted.len()
        );
        for (depth, sym) in &impacted {
            let _ = writeln!(
                out,
                "  d={depth} [{}] {} ({}:{})",
                sym.sym.kind.as_str(),
                sym.sym.name,
                sym.rel_path,
                sym.start_line
            );
        }
        out.push_str("</impact>");
        Ok((out, impacted.len()))
    })
}

/// Native `connect` operation: shortest undirected graph path between `a` and `b`.
pub(super) fn native_connect(
    ctx: &ToolContext,
    target_path: &Path,
    from_query: &str,
    to_query: &str,
) -> Result<(String, usize), ToolError> {
    let root = tree_root_for(ctx, target_path);
    with_index(ctx, &root, |ws| {
        let scope = relative_scope(&root, target_path);
        let starts = ws.find_matching_symbols(scope.as_deref(), from_query);
        let targets: HashSet<usize> = ws
            .find_matching_symbols(scope.as_deref(), to_query)
            .into_iter()
            .collect();

        if starts.is_empty() || targets.is_empty() {
            return Ok((
                format!("No path: could not resolve symbols for '{from_query}' and '{to_query}'."),
                0,
            ));
        }

        let mut parent: HashMap<usize, usize> = HashMap::new();
        let mut visited: HashSet<usize> = starts.iter().copied().collect();
        let mut queue: VecDeque<usize> = starts.iter().copied().collect();
        let mut reached = None;

        while let Some(curr) = queue.pop_front() {
            if targets.contains(&curr) && !starts.contains(&curr) {
                reached = Some(curr);
                break;
            }
            for &next in ws.callees[curr].iter().chain(ws.callers[curr].iter()) {
                if visited.insert(next) {
                    parent.insert(next, curr);
                    if targets.contains(&next) {
                        reached = Some(next);
                        break;
                    }
                    queue.push_back(next);
                }
            }
            if reached.is_some() {
                break;
            }
        }

        let Some(end) = reached else {
            return Ok((
                format!("No call-graph path found between '{from_query}' and '{to_query}'."),
                0,
            ));
        };

        let mut path = vec![end];
        let mut cur = end;
        while let Some(&prev) = parent.get(&cur) {
            path.push(prev);
            cur = prev;
        }
        path.reverse();

        let hops = path.len().saturating_sub(1);
        let mut out =
            format!("<connect from=\"{from_query}\" to=\"{to_query}\" hops=\"{hops}\">\n");
        for (idx, &node_id) in path.iter().enumerate() {
            let s = &ws.all_symbols[node_id];
            let _ = writeln!(
                out,
                "  {}. {} [{}] ({}:{})",
                idx + 1,
                s.sym.name,
                s.sym.kind.as_str(),
                s.rel_path,
                s.start_line
            );
        }
        out.push_str("</connect>");
        Ok((out, hops))
    })
}

/// Native `communities` operation: module directory clusters, hubs, and cross-module bridges.
pub(super) fn native_communities(
    ctx: &ToolContext,
    target_path: &Path,
    filter: Option<&str>,
    limit: usize,
) -> Result<(String, usize), ToolError> {
    let root = tree_root_for(ctx, target_path);
    with_index(ctx, &root, |ws| {
        let scope = relative_scope(&root, target_path);
        let mut clusters: HashMap<String, Vec<&IndexedSymbol>> = HashMap::new();
        let mut sym_cluster: HashMap<usize, String> = HashMap::new();

        for sym in &ws.all_symbols {
            if !path_matches_scope(&sym.rel_path, scope.as_deref()) {
                continue;
            }
            let dir = cluster_name(&sym.rel_path);
            sym_cluster.insert(sym.id, dir.clone());
            clusters.entry(dir).or_default().push(sym);
        }

        let mut bridges: HashMap<(String, String), usize> = HashMap::new();
        for (caller_id, callees) in ws.callees.iter().enumerate() {
            let Some(from_c) = sym_cluster.get(&caller_id) else {
                continue;
            };
            for callee_id in callees {
                let Some(to_c) = sym_cluster.get(callee_id) else {
                    continue;
                };
                if from_c != to_c {
                    let pair = if from_c <= to_c {
                        (from_c.clone(), to_c.clone())
                    } else {
                        (to_c.clone(), from_c.clone())
                    };
                    *bridges.entry(pair).or_default() += 1;
                }
            }
        }

        let filter_low = filter
            .map(|f| f.trim().to_lowercase())
            .filter(|f| !f.is_empty());
        let mut sorted: Vec<(String, Vec<&IndexedSymbol>)> = clusters
            .into_iter()
            .filter(|(name, _)| {
                filter_low
                    .as_ref()
                    .is_none_or(|f| name.to_lowercase().contains(f))
            })
            .collect();
        sorted.sort_by(|a, b| b.1.len().cmp(&a.1.len()).then_with(|| a.0.cmp(&b.0)));

        let mut out = format!(
            "<communities modules=\"{}\" symbols=\"{}\">\n",
            sorted.len(),
            ws.all_symbols.len()
        );
        let mut emitted = 0usize;
        for (name, mut syms) in sorted.into_iter().take(limit) {
            syms.sort_by(|a, b| {
                (ws.fan_in(b.id) + ws.fan_out(b.id)).cmp(&(ws.fan_in(a.id) + ws.fan_out(a.id)))
            });
            let hubs: Vec<String> = syms
                .iter()
                .take(4)
                .map(|s| format!("{}({}:{})", s.sym.name, s.rel_path, s.start_line))
                .collect();
            let _ = writeln!(
                out,
                "  <community id=\"{name}\" size=\"{}\" hubs=\"{}\"/>",
                syms.len(),
                hubs.join(", ")
            );
            emitted += 1;
        }

        let mut sorted_bridges: Vec<((String, String), usize)> = bridges.into_iter().collect();
        sorted_bridges.sort_by_key(|a| std::cmp::Reverse(a.1));
        for ((a, b), count) in sorted_bridges.into_iter().take(12) {
            let _ = writeln!(out, "  <bridge from=\"{a}\" to=\"{b}\" edges=\"{count}\"/>");
        }
        out.push_str("</communities>");
        Ok((out, emitted))
    })
}

fn cluster_name(rel_path: &str) -> String {
    let parts: Vec<&str> = rel_path.split('/').collect();
    if parts.len() <= 1 {
        "(root)".to_string()
    } else if parts.len() == 2 {
        parts[0].to_string()
    } else {
        format!("{}/{}", parts[0], parts[1])
    }
}

/// Native `affected` operation: discover test files impacted by files or symbols.
pub(super) fn native_affected(
    ctx: &ToolContext,
    target_path: &Path,
    seed_csv: &str,
    limit: usize,
) -> Result<(String, usize), ToolError> {
    let root = tree_root_for(ctx, target_path);
    with_index(ctx, &root, |ws| {
        let scope = relative_scope(&root, target_path);
        let raw_seeds: Vec<String> = seed_csv
            .split([',', '\n'])
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
            .collect();

        let mut seed_ids: HashSet<usize> = HashSet::new();
        for seed in &raw_seeds {
            let seed_norm = seed.replace('\\', "/").to_lowercase();
            let mut matched_file = false;
            for sym in ws.symbols_in_scope(&root, target_path) {
                if sym.rel_path.to_lowercase().ends_with(&seed_norm)
                    || sym.rel_path.to_lowercase().contains(&seed_norm)
                {
                    seed_ids.insert(sym.id);
                    matched_file = true;
                }
            }
            if !matched_file {
                for id in ws.find_matching_symbols(scope.as_deref(), seed) {
                    seed_ids.insert(id);
                }
            }
        }

        // Reverse BFS to find callers and test files/functions.
        let mut visited = seed_ids.clone();
        let mut queue: VecDeque<usize> = seed_ids.into_iter().collect();
        while let Some(node) = queue.pop_front() {
            for &caller in &ws.callers[node] {
                if visited.insert(caller) {
                    queue.push_back(caller);
                }
            }
        }

        let mut affected_tests: Vec<String> = Vec::new();
        let mut seen_tests = HashSet::new();
        for &id in &visited {
            let s = &ws.all_symbols[id];
            if is_test_path_or_symbol(&s.rel_path, &s.sym.name)
                && seen_tests.insert(format!("{}:{}", s.rel_path, s.sym.name))
            {
                affected_tests.push(format!(
                    "  test {} ({}:{})",
                    s.sym.name, s.rel_path, s.start_line
                ));
                if affected_tests.len() >= limit {
                    break;
                }
            }
        }

        // Also match test files whose text contains seeded symbol names when edge resolution misses.
        if affected_tests.len() < limit && !raw_seeds.is_empty() {
            for path in &ws.file_order {
                let Some(file) = ws.files_by_path.get(path) else {
                    continue;
                };
                if !path_matches_scope(&file.rel_path, scope.as_deref()) {
                    continue;
                }
                if !is_test_path_or_symbol(&file.rel_path, "") {
                    continue;
                }
                let joined = file.lines.join("\n");
                let hits = raw_seeds.iter().any(|seed| {
                    let stem = Path::new(seed)
                        .file_stem()
                        .and_then(|s| s.to_str())
                        .unwrap_or(seed);
                    !stem.is_empty() && joined.contains(stem)
                });
                if hits && seen_tests.insert(file.rel_path.clone()) {
                    affected_tests.push(format!("  test_file {}", file.rel_path));
                    if affected_tests.len() >= limit {
                        break;
                    }
                }
            }
        }

        let count = affected_tests.len();
        let body = if affected_tests.is_empty() {
            format!("<affected seeds=\"{seed_csv}\" tests=\"0\"/>")
        } else {
            format!(
                "<affected seeds=\"{seed_csv}\" tests=\"{count}\">\n{}\n</affected>",
                affected_tests.join("\n")
            )
        };
        Ok((body, count))
    })
}

fn is_test_path_or_symbol(rel_path: &str, name: &str) -> bool {
    let p = rel_path.to_lowercase();
    let n = name.to_lowercase();
    p.contains("/tests/")
        || p.starts_with("tests/")
        || p.contains("_test.")
        || p.contains(".test.")
        || p.contains(".spec.")
        || n.starts_with("test_")
}

/// Native `trace` operation: parse `path:line` frames from a stack trace and
/// map each to its enclosing symbol + body within `max_tokens`.
pub(super) fn native_trace(
    ctx: &ToolContext,
    target_path: &Path,
    trace_text: &str,
    max_tokens: usize,
) -> Result<(String, usize), ToolError> {
    let root = tree_root_for(ctx, target_path);
    with_index(ctx, &root, |ws| {
        let frames = extract_trace_frames(trace_text);
        if frames.is_empty() {
            return Ok((
                "No `file:line` stack frames detected in trace input.".to_string(),
                0,
            ));
        }

        let max_chars = max_tokens.saturating_mul(4);
        let mut out = String::from("<trace>\n");
        let mut resolved = 0usize;

        for (file_ref, line_num) in frames {
            let ids = ws.find_by_file_line(&file_ref, line_num);
            if let Some(&sym_id) = ids.first() {
                let sym = &ws.all_symbols[sym_id];
                let file_path = root.join(&sym.rel_path);
                let snippet = ws
                    .files_by_path
                    .get(&file_path)
                    .map_or_else(String::new, |f| {
                        let start = sym.start_line.saturating_sub(1);
                        let end = sym.end_line.min(f.lines.len()).min(start + 24);
                        f.lines[start..end].join("\n")
                    });
                let entry = format!(
                    "  <frame file=\"{}:{line_num}\" symbol=\"{}\" span=\"{}..{}\">\n{}\n  </frame>\n",
                    sym.rel_path, sym.sym.name, sym.start_line, sym.end_line, snippet
                );
                if out.len() + entry.len() <= max_chars || resolved == 0 {
                    out.push_str(&entry);
                    resolved += 1;
                }
            }
        }
        out.push_str("</trace>");
        Ok((out, resolved))
    })
}

fn extract_trace_frames(trace_text: &str) -> Vec<(String, usize)> {
    let mut frames = Vec::new();
    let mut seen = HashSet::new();
    for raw in trace_text.split_whitespace() {
        let trimmed =
            raw.trim_matches(|c: char| matches!(c, '(' | ')' | '[' | ']' | ',' | ';' | '"' | '\''));
        let parts: Vec<&str> = trimmed.split(':').collect();
        if parts.len() >= 2 {
            // Match either `path:line` or `path:line:col`.
            for win in parts.windows(2) {
                if let Ok(line_num) = win[1].parse::<usize>()
                    && line_num > 0
                    && (win[0].contains('.') || win[0].contains('/') || win[0].contains('\\'))
                    && seen.insert((win[0].to_string(), line_num))
                {
                    frames.push((win[0].to_string(), line_num));
                    break;
                }
            }
        }
    }
    frames
}

/// Native `pack_task`: budgeted focal signatures + 1-hop graph neighbors + compact bodies.
pub(super) fn native_pack_task(
    ctx: &ToolContext,
    target_path: &Path,
    task: &str,
    max_tokens: usize,
    limit: usize,
) -> Result<(String, usize), ToolError> {
    let root = tree_root_for(ctx, target_path);
    with_index(ctx, &root, |ws| {
        let scope = relative_scope(&root, target_path);
        let q_terms: Vec<String> = task
            .split_whitespace()
            .map(|t| t.trim().to_lowercase())
            .filter(|t| !t.is_empty())
            .collect();

        let mut scored: Vec<(f32, usize)> = Vec::new();
        for sym in &ws.all_symbols {
            if !path_matches_scope(&sym.rel_path, scope.as_deref()) {
                continue;
            }
            let name_low = sym.sym.name.to_lowercase();
            let sig_low = sym.sym.signature.to_lowercase();
            let path_low = sym.rel_path.to_lowercase();
            let mut weight = 0.0_f32;
            for term in &q_terms {
                if name_low == *term {
                    weight += 15.0;
                } else if name_low.contains(term) {
                    weight += 8.0;
                }
                if sig_low.contains(term) {
                    weight += 3.0;
                }
                if path_low.contains(term) {
                    weight += 4.0;
                }
            }
            if weight > 0.0 {
                weight += usize_to_f32(ws.fan_in(sym.id)) * 1.2;
                scored.push((weight, sym.id));
            }
        }
        scored.sort_by(|a, b| b.0.total_cmp(&a.0));

        let max_chars = max_tokens.saturating_mul(4).max(400);
        let mut out = format!("<ctx task=\"{task}\">\n<sigs>\n");
        let mut focal_ids = Vec::new();

        for (_, id) in scored.into_iter().take(limit.min(16)) {
            let s = &ws.all_symbols[id];
            let sig_line = format!(
                "  {}:{} [{}] {}\n",
                s.rel_path,
                s.start_line,
                s.sym.kind.as_str(),
                s.sym.signature
            );
            if out.len() + sig_line.len() > max_chars / 2 && !focal_ids.is_empty() {
                break;
            }
            out.push_str(&sig_line);
            focal_ids.push(id);
        }
        out.push_str("</sigs>\n<bodies>\n");

        for id in focal_ids.iter().take(4) {
            if out.len() >= max_chars {
                break;
            }
            let sym = &ws.all_symbols[*id];
            let file_path = root.join(&sym.rel_path);
            let Some(file) = ws.files_by_path.get(&file_path) else {
                continue;
            };
            let start = sym.start_line.saturating_sub(1);
            let end = sym.end_line.min(f_end(file.lines.len(), start));
            let slice = strip_comments_and_blank_runs(&file.lines[start..end].join("\n"));
            let entry = format!(
                "  <body symbol=\"{}\" file=\"{}:{}\">\n{}\n  </body>\n",
                sym.sym.name, sym.rel_path, sym.start_line, slice
            );
            if out.len() + entry.len() <= max_chars || out.len() < 400 {
                out.push_str(&entry);
            }
        }
        out.push_str("</bodies>\n</ctx>");
        Ok((out, focal_ids.len()))
    })
}

fn f_end(len: usize, start: usize) -> usize {
    len.min(start.saturating_add(40))
}

/// Resolved 1-based `(file_path, start_line, end_line, name, kind)` for a native
/// symbol lookup, used by LSP precision anchoring and atomic native symbol edits.
#[derive(Debug, Clone)]
pub(crate) struct NativeSymbolLocation {
    pub(crate) abs_path: PathBuf,
    pub(crate) name: String,
    pub(crate) kind: SymbolKind,
    pub(crate) start_line: usize,
    pub(crate) end_line: usize,
    pub(crate) signature: String,
}

/// Partition slice computed either natively or from `ripwire`.
#[derive(Debug, Clone)]
pub(crate) struct NativePartitionSlice {
    pub(crate) index: usize,
    pub(crate) files: Vec<String>,
    pub(crate) symbols: Vec<String>,
    pub(crate) text: String,
}

/// Locate a symbol's declaration `(abs_path, 1-based line, 1-based column)` for
/// LSP precision queries.
pub(super) fn locate_symbol_line_col(
    ctx: &ToolContext,
    target_path: &Path,
    query: &str,
) -> Option<(PathBuf, u32, u32)> {
    let root = tree_root_for(ctx, target_path);
    with_index(ctx, &root, |ws| {
        let scope = relative_scope(&root, target_path);
        let ids = ws.find_matching_symbols(scope.as_deref(), query);
        let Some(&id) = ids.first() else {
            return Ok(None);
        };
        let sym = &ws.all_symbols[id];
        let abs = root.join(&sym.rel_path);
        let line_1 = sym.start_line.max(1);
        let col_1 = ws
            .files_by_path
            .get(&abs)
            .and_then(|f| f.lines.get(line_1 - 1))
            .and_then(|line_text| {
                let short = sym
                    .sym
                    .name
                    .rsplit([':', '.'])
                    .next()
                    .unwrap_or(&sym.sym.name);
                line_text.find(short).map(|byte_off| {
                    line_text[..byte_off]
                        .encode_utf16()
                        .count()
                        .saturating_add(1)
                })
            })
            .unwrap_or(1);
        Ok(Some((
            abs,
            u32::try_from(line_1).unwrap_or(u32::MAX),
            u32::try_from(col_1).unwrap_or(1),
        )))
    })
    .ok()
    .flatten()
}

/// Resolve all matching definitions for a symbol query (`symbol`, `file:symbol`,
/// or `@FILE:LINE`), optionally confined to `target_file`.
pub(crate) fn find_native_symbol_locations(
    ctx: &ToolContext,
    target_path: &Path,
    query: &str,
) -> Result<Vec<NativeSymbolLocation>, ToolError> {
    let root = tree_root_for(ctx, target_path);
    with_index(ctx, &root, |ws| {
        let scope = relative_scope(&root, target_path);
        let ids = ws.find_matching_symbols(scope.as_deref(), query);
        let mut out = Vec::with_capacity(ids.len());
        for id in ids {
            let sym = &ws.all_symbols[id];
            let abs_path = root.join(&sym.rel_path);
            let refined_end = fs::read_to_string(&abs_path).map_or(sym.end_line, |src| {
                let ext = abs_path.extension().and_then(|e| e.to_str()).unwrap_or("");
                compute_refined_end_line(&src, ext, sym.start_line, sym.end_line)
            });
            out.push(NativeSymbolLocation {
                abs_path,
                name: sym.sym.name.clone(),
                kind: sym.sym.kind,
                start_line: sym.start_line,
                end_line: refined_end,
                signature: sym.sym.signature.clone(),
            });
        }
        Ok(out)
    })
}

/// String/comment-aware symbol end line calculation for atomic fallback edits.
fn compute_refined_end_line(
    source: &str,
    ext: &str,
    start_line: usize,
    fallback_end: usize,
) -> usize {
    if matches!(ext.to_ascii_lowercase().as_str(), "py" | "md" | "markdown") {
        return fallback_end;
    }
    let Some(start_byte) = line_byte_offset(source, start_line) else {
        return fallback_end;
    };
    let bytes = source.as_bytes();
    let mut i = start_byte;
    let mut depth: i32 = 0;
    let mut saw_open = false;
    let mut in_line_comment = false;
    let mut block_comment_depth: usize = 0;
    let mut in_string: Option<u8> = None;
    let mut current_line = start_line;

    while i < bytes.len() {
        let b = bytes[i];
        if b == b'\n' {
            current_line += 1;
            in_line_comment = false;
            i += 1;
            continue;
        }
        if in_line_comment {
            i += 1;
            continue;
        }
        if block_comment_depth > 0 {
            if b == b'/' && i + 1 < bytes.len() && bytes[i + 1] == b'*' {
                block_comment_depth += 1;
                i += 2;
                continue;
            }
            if b == b'*' && i + 1 < bytes.len() && bytes[i + 1] == b'/' {
                block_comment_depth -= 1;
                i += 2;
                continue;
            }
            i += 1;
            continue;
        }
        if let Some(quote) = in_string {
            if b == b'\\' && i + 1 < bytes.len() {
                i += 2;
                continue;
            }
            if b == quote {
                in_string = None;
            }
            i += 1;
            continue;
        }
        if b == b'/' && i + 1 < bytes.len() {
            if bytes[i + 1] == b'/' {
                in_line_comment = true;
                i += 2;
                continue;
            }
            if bytes[i + 1] == b'*' {
                block_comment_depth = 1;
                i += 2;
                continue;
            }
        }
        if ext == "rs" && b == b'\'' {
            if i + 3 < bytes.len() && bytes[i + 1] == b'\\' && bytes[i + 3] == b'\'' {
                i += 4;
                continue;
            }
            if i + 2 < bytes.len()
                && bytes[i + 1] != b'\\'
                && bytes[i + 1] != b'\''
                && bytes[i + 1] != b'\n'
                && bytes[i + 2] == b'\''
            {
                i += 3;
                continue;
            }
        }
        if b == b'"' || (b == b'\'' && ext != "rs") {
            in_string = Some(b);
            i += 1;
            continue;
        }
        if b == b'{' {
            depth += 1;
            saw_open = true;
        } else if b == b'}' {
            depth = depth.saturating_sub(1);
            if saw_open && depth == 0 {
                return current_line;
            }
        } else if b == b';' && !saw_open && depth == 0 {
            return current_line;
        }
        i += 1;
    }
    fallback_end
}

fn line_byte_offset(source: &str, line_1: usize) -> Option<usize> {
    if line_1 <= 1 {
        return Some(0);
    }
    let mut cur = 1usize;
    for (idx, b) in source.bytes().enumerate() {
        if b == b'\n' {
            cur += 1;
            if cur == line_1 {
                return Some(idx + 1);
            }
        }
    }
    None
}

/// Fast pure-Rust impacted test discovery for a list of changed workspace paths.
pub(crate) fn native_affected_tests_for_files(
    ctx: &ToolContext,
    root: &Path,
    changed_files: &[String],
) -> Vec<String> {
    with_index(ctx, root, |ws| {
        let mut seed_ids = Vec::new();
        for changed in changed_files {
            let norm = changed.replace('\\', "/");
            let norm = norm.strip_prefix("./").unwrap_or(&norm);
            if norm.is_empty() {
                continue;
            }
            for sym in &ws.all_symbols {
                let matched = sym.rel_path == norm
                    || sym.rel_path.ends_with(&format!("/{norm}"))
                    || norm.ends_with(&format!("/{}", sym.rel_path));
                if matched {
                    seed_ids.push(sym.id);
                }
            }
        }
        let mut visited = HashSet::new();
        let mut queue = VecDeque::new();
        for id in seed_ids {
            if visited.insert(id) {
                queue.push_back((id, 0usize));
            }
        }
        let mut test_files = BTreeSet::new();
        while let Some((cur, depth)) = queue.pop_front() {
            let s = &ws.all_symbols[cur];
            if is_test_path_or_symbol(&s.rel_path, &s.sym.name) {
                test_files.insert(s.rel_path.clone());
            }
            if depth >= 4 {
                continue;
            }
            for &caller_id in &ws.callers[cur] {
                if visited.insert(caller_id) {
                    queue.push_back((caller_id, depth + 1));
                }
            }
        }
        Ok(test_files.into_iter().take(12).collect())
    })
    .unwrap_or_default()
}

type FileScoreMap<'a> = BTreeMap<String, (f32, Vec<&'a IndexedSymbol>)>;
type PartitionBucket<'a> = (f32, Vec<(String, Vec<&'a IndexedSymbol>)>);

/// Disjoint graph partitioning fallback when `ripwire` is absent.
pub(crate) fn native_partition_task(
    ctx: &ToolContext,
    target_path: &Path,
    task: &str,
    partitions: usize,
    max_tokens: usize,
) -> Result<(String, Vec<NativePartitionSlice>, String), ToolError> {
    let partitions = partitions.clamp(2, 16);
    let root = tree_root_for(ctx, target_path);
    with_index(ctx, &root, |ws| {
        let scope = relative_scope(&root, target_path);
        let q_terms: Vec<String> = task
            .split_whitespace()
            .map(|t| t.trim().to_lowercase())
            .filter(|t| t.len() >= 2)
            .collect();

        let (file_scores, mut hubs) = score_files_for_partition(ws, scope.as_deref(), &q_terms);
        hubs.sort_by_key(|a| std::cmp::Reverse(a.0));
        let core_text = render_partition_core(&hubs, max_tokens);

        let mut ranked_files: Vec<(String, f32, Vec<&IndexedSymbol>)> = file_scores
            .into_iter()
            .map(|(f, (s, syms))| (f, s, syms))
            .collect();
        ranked_files.sort_by(|a, b| b.1.total_cmp(&a.1).then_with(|| a.0.cmp(&b.0)));

        let slices = pack_partition_buckets(ranked_files, partitions, max_tokens);
        Ok((core_text, slices, "native".to_string()))
    })
}

fn score_files_for_partition<'a>(
    ws: &'a CachedWorkspace,
    scope: Option<&str>,
    q_terms: &[String],
) -> (FileScoreMap<'a>, Vec<(usize, &'a IndexedSymbol)>) {
    let mut file_scores: FileScoreMap<'a> = BTreeMap::new();
    let mut hubs: Vec<(usize, &IndexedSymbol)> = Vec::new();
    for sym in &ws.all_symbols {
        if !path_matches_scope(&sym.rel_path, scope) {
            continue;
        }
        let degree = ws.fan_in(sym.id) + ws.fan_out(sym.id);
        if degree >= 2 {
            hubs.push((degree, sym));
        }
        let name_low = sym.sym.name.to_lowercase();
        let path_low = sym.rel_path.to_lowercase();
        let sig_low = sym.sym.signature.to_lowercase();
        let mut rel = 0.5_f32 + usize_to_f32(degree) * 0.25;
        for term in q_terms {
            if name_low.contains(term) {
                rel += 8.0;
            }
            if sig_low.contains(term) {
                rel += 3.0;
            }
            if path_low.contains(term) {
                rel += 4.0;
            }
        }
        let entry = file_scores
            .entry(sym.rel_path.clone())
            .or_insert_with(|| (0.0, Vec::new()));
        entry.0 += rel;
        entry.1.push(sym);
    }
    (file_scores, hubs)
}

fn render_partition_core(hubs: &[(usize, &IndexedSymbol)], max_tokens: usize) -> String {
    let mut core_text = String::from("<core>\n");
    let core_budget = (max_tokens * 2).max(300);
    for (_, h) in hubs.iter().take(8) {
        let line = format!(
            "  {}:{} [{}] {}\n",
            h.rel_path,
            h.start_line,
            h.sym.kind.as_str(),
            h.sym.signature
        );
        if core_text.len() + line.len() <= core_budget {
            core_text.push_str(&line);
        }
    }
    core_text.push_str("</core>");
    core_text
}

fn pack_partition_buckets(
    ranked_files: Vec<(String, f32, Vec<&IndexedSymbol>)>,
    partitions: usize,
    max_tokens: usize,
) -> Vec<NativePartitionSlice> {
    let mut buckets: Vec<PartitionBucket<'_>> = vec![(0.0, Vec::new()); partitions];
    for (file, score, syms) in ranked_files.into_iter().take(partitions.saturating_mul(6)) {
        if let Some(target_idx) = buckets
            .iter()
            .enumerate()
            .min_by(|(_, a), (_, b)| a.0.total_cmp(&b.0))
            .map(|(i, _)| i)
        {
            buckets[target_idx].0 += score;
            buckets[target_idx].1.push((file, syms));
        }
    }

    let per_slice_chars = max_tokens.saturating_mul(4).max(300);
    let mut slices = Vec::with_capacity(partitions);
    for (idx, (_, assigned)) in buckets.into_iter().enumerate() {
        let mut files = Vec::new();
        let mut symbols = Vec::new();
        let mut text = format!("<partition i=\"{idx}\">\n");
        for (rel_file, syms) in assigned {
            files.push(rel_file.clone());
            let _ = writeln!(text, "  <f p=\"{rel_file}\">");
            for sym in syms.into_iter().take(8) {
                symbols.push(sym.sym.name.clone());
                let row = format!(
                    "    L{:<4} [{}] {}\n",
                    sym.start_line,
                    sym.sym.kind.as_str(),
                    sym.sym.signature
                );
                if text.len() + row.len() <= per_slice_chars {
                    text.push_str(&row);
                }
            }
            text.push_str("  </f>\n");
        }
        text.push_str("</partition>");
        slices.push(NativePartitionSlice {
            index: idx,
            files,
            symbols,
            text,
        });
    }
    slices
}

/// Native `lego`: find interface/trait contract and all implementing types.
pub(super) fn native_lego(
    ctx: &ToolContext,
    target_path: &Path,
    type_name: &str,
    limit: usize,
) -> Result<(String, usize), ToolError> {
    let root = tree_root_for(ctx, target_path);
    let needle = type_name.trim();
    let short_needle = needle.rsplit([':', '.']).next().unwrap_or(needle);
    let needle_low = short_needle.to_lowercase();

    with_index(ctx, &root, |ws| {
        let scope = relative_scope(&root, target_path);
        let mut def_lines = Vec::new();
        let mut impls: BTreeMap<String, (String, usize, String)> = BTreeMap::new();

        for path in &ws.file_order {
            let Some(file) = ws.files_by_path.get(path) else {
                continue;
            };
            if !path_matches_scope(&file.rel_path, scope.as_deref()) {
                continue;
            }
            for sym in &file.symbols {
                if sym.sym.name.eq_ignore_ascii_case(short_needle)
                    && matches!(
                        sym.sym.kind,
                        SymbolKind::Trait
                            | SymbolKind::Interface
                            | SymbolKind::Class
                            | SymbolKind::Struct
                    )
                {
                    def_lines.push(format!(
                        "{}:{} [{}] {}",
                        sym.rel_path,
                        sym.start_line,
                        sym.sym.kind.as_str(),
                        sym.sym.signature
                    ));
                }
                if sym.sym.kind == SymbolKind::Class {
                    let name_low = sym.sym.name.to_lowercase();
                    if name_low.starts_with(&format!("{needle_low} for ")) {
                        let impl_name = sym
                            .sym
                            .name
                            .split_once(" for ")
                            .map_or(sym.sym.name.as_str(), |(_, rhs)| rhs.trim())
                            .to_string();
                        impls.entry(impl_name.clone()).or_insert_with(|| {
                            (
                                sym.rel_path.clone(),
                                sym.start_line,
                                sym.sym.signature.clone(),
                            )
                        });
                    }
                }
            }

            for (idx, line) in file.lines.iter().enumerate() {
                if !line.contains(short_needle) {
                    continue;
                }
                let trimmed = line.trim();
                let is_impl_line = trimmed.starts_with("impl ")
                    || trimmed.starts_with("impl<")
                    || trimmed.contains(&format!("implements {short_needle}"))
                    || trimmed.contains(&format!("extends {short_needle}"))
                    || (trimmed.starts_with("class ")
                        && trimmed.contains(&format!("({short_needle}")));
                if is_impl_line {
                    let key = format!("{}:{}", file.rel_path, idx + 1);
                    impls
                        .entry(key)
                        .or_insert_with(|| (file.rel_path.clone(), idx + 1, trimmed.to_string()));
                }
            }
        }

        let total = impls.len();
        let mut out = format!("Interface `{short_needle}` (implementors: {total}):\n");
        if def_lines.is_empty() {
            let _ = writeln!(out, "  definition: (not uniquely indexed in scope)");
        } else {
            for d in def_lines.iter().take(4) {
                let _ = writeln!(out, "  definition: {d}");
            }
        }
        for (_, (rel, line_no, sig)) in impls.into_iter().take(limit) {
            let _ = writeln!(out, "  - {rel}:{line_no} {sig}");
        }
        Ok((out, total))
    })
}

/// Native `slice`: intra-procedural def-use variable slice inside one function.
pub(super) fn native_slice(
    ctx: &ToolContext,
    target_path: &Path,
    sym_query: &str,
    var: Option<&str>,
    flow: Option<&str>,
    depth: usize,
) -> Result<(String, usize), ToolError> {
    let root = tree_root_for(ctx, target_path);
    with_index(ctx, &root, |ws| {
        let scope = relative_scope(&root, target_path);
        let (sym_part, inline_var) = sym_query
            .rsplit_once(':')
            .filter(|(lhs, rhs)| {
                !lhs.is_empty() && !rhs.is_empty() && rhs.parse::<usize>().is_err()
            })
            .map_or((sym_query, None), |(l, r)| (l, Some(r)));
        let target_var = var.or(inline_var).map(str::trim).filter(|s| !s.is_empty());

        let ids = ws.find_matching_symbols(scope.as_deref(), sym_part);
        let Some(&id) = ids.first() else {
            return Ok((format!("Symbol `{sym_part}` not found for slice."), 0));
        };
        let sym = &ws.all_symbols[id];
        let abs = root.join(&sym.rel_path);
        let Some(file) = ws.files_by_path.get(&abs) else {
            return Ok((format!("Source unavailable for {}", sym.rel_path), 0));
        };
        let start = sym.start_line.saturating_sub(1);
        let end = sym.end_line.min(file.lines.len());
        let body_lines = &file.lines[start..end];

        let mut vars_seen = BTreeSet::new();
        for line in body_lines {
            for ident in extract_call_idents(line) {
                if ident.chars().next().is_some_and(char::is_lowercase) {
                    vars_seen.insert(ident);
                }
            }
        }

        let Some(var_name) = target_var else {
            let list: Vec<String> = vars_seen.into_iter().take(32).collect();
            return Ok((
                format!(
                    "Sliceable locals in `{}` ({}:{}..{}):\n  {}",
                    sym.sym.name,
                    sym.rel_path,
                    sym.start_line,
                    sym.end_line,
                    list.join(", ")
                ),
                list.len(),
            ));
        };

        let mut rows = Vec::new();
        for (offset, line) in body_lines.iter().enumerate() {
            if contains_ident_token(line, var_name) {
                let line_no = sym.start_line + offset;
                let trimmed = line.trim();
                let is_def = trimmed.starts_with(&format!("let {var_name}"))
                    || trimmed.starts_with(&format!("let mut {var_name}"))
                    || trimmed.contains(&format!("{var_name} = "))
                    || trimmed.contains(&format!("{var_name} :="));
                let kind = if is_def && offset > 0 {
                    "def"
                } else if offset == 0 {
                    "param"
                } else {
                    "use"
                };
                rows.push(format!("  L{line_no:<4} [{kind}] {trimmed}"));
            }
        }
        let flow_label = flow.unwrap_or("linear");
        let capped = rows
            .into_iter()
            .take(depth.saturating_mul(6).max(20))
            .collect::<Vec<_>>();
        let count = capped.len();
        let out = format!(
            "Slice `{}:{}` ({}:{}, flow={flow_label}, hits={count}):\n{}",
            sym.sym.name,
            var_name,
            sym.rel_path,
            sym.start_line,
            capped.join("\n")
        );
        Ok((out, count))
    })
}

fn contains_ident_token(line: &str, needle: &str) -> bool {
    extract_call_idents(line).iter().any(|tok| tok == needle)
}

/// Native `cochange`: compute git historical co-edit partners for a file.
pub(super) fn native_cochange(
    ctx: &ToolContext,
    target_path: &Path,
    limit: usize,
) -> (String, usize) {
    let root = tree_root_for(ctx, target_path);
    let target_rel = relative_to(&root, target_path);
    let hashes_out = Command::new("git")
        .arg("-C")
        .arg(&root)
        .args(["log", "-n", "60", "--format=%H", "--", &target_rel])
        .output();
    let Ok(hashes_res) = hashes_out else {
        return (format!("No git history available for `{target_rel}`."), 0);
    };
    if !hashes_res.status.success() {
        return (format!("No git co-change history for `{target_rel}`."), 0);
    }
    let hashes: Vec<String> = String::from_utf8_lossy(&hashes_res.stdout)
        .lines()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(ToString::to_string)
        .collect();
    if hashes.is_empty() {
        return (format!("0 commits found touching `{target_rel}`."), 0);
    }

    let mut partner_counts: HashMap<String, usize> = HashMap::new();
    if let Ok(mut child) = Command::new("git")
        .arg("-C")
        .arg(&root)
        .args([
            "diff-tree",
            "--stdin",
            "--no-commit-id",
            "--name-only",
            "-r",
            "--root",
        ])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
    {
        if let Some(mut stdin) = child.stdin.take() {
            let mut payload = hashes.join("\n");
            payload.push('\n');
            let _ = stdin.write_all(payload.as_bytes());
        }
        if let Ok(files_res) = child.wait_with_output()
            && files_res.status.success()
        {
            for f in String::from_utf8_lossy(&files_res.stdout).lines() {
                let f = f.trim();
                if !f.is_empty() && f != target_rel {
                    *partner_counts.entry(f.to_string()).or_default() += 1;
                }
            }
        }
    }

    let mut ranked: Vec<(String, usize)> = partner_counts.into_iter().collect();
    ranked.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
    let total = ranked.len();
    let mut text = format!(
        "Co-change partners for `{target_rel}` (over {} commits, {} partners):\n",
        hashes.len(),
        total
    );
    for (file, cnt) in ranked.into_iter().take(limit) {
        let _ = writeln!(text, "  {cnt:>3}x  {file}");
    }
    (text, total)
}

/// Native `doc_drift`: verify backticked path/symbol anchors in markdown docs.
pub(super) fn native_doc_drift(
    ctx: &ToolContext,
    target_path: &Path,
    filter: Option<&str>,
    limit: usize,
) -> Result<(String, usize), ToolError> {
    let root = tree_root_for(ctx, target_path);
    with_index(ctx, &root, |ws| {
        let scope = relative_scope(&root, target_path);
        let mut indexed_paths: HashSet<String> = HashSet::new();
        for p in &ws.file_order {
            if let Some(f) = ws.files_by_path.get(p) {
                indexed_paths.insert(f.rel_path.clone());
            }
        }

        let mut findings = Vec::new();
        for p in &ws.file_order {
            let Some(file) = ws.files_by_path.get(p) else {
                continue;
            };
            if !matches!(file.ext.to_ascii_lowercase().as_str(), "md" | "markdown") {
                continue;
            }
            if !path_matches_scope(&file.rel_path, scope.as_deref()) {
                continue;
            }
            if let Some(sub) = filter
                && !file.rel_path.contains(sub)
            {
                continue;
            }
            let mut in_fence = false;
            for (idx, line) in file.lines.iter().enumerate() {
                if line.trim_start().starts_with("```") {
                    in_fence = !in_fence;
                    continue;
                }
                if in_fence {
                    continue;
                }
                for token in extract_backticked_tokens(line) {
                    if looks_like_repo_path(&token)
                        && !root.join(&token).exists()
                        && !indexed_paths.iter().any(|ip| ip.ends_with(&token))
                    {
                        findings.push(format!(
                            "  {}:{} missing path `{token}`",
                            file.rel_path,
                            idx + 1
                        ));
                    }
                }
            }
        }

        let total = findings.len();
        let mut out = format!("Doc drift check ({total} missing path anchors):\n");
        for row in findings.into_iter().take(limit) {
            out.push_str(&row);
            out.push('\n');
        }
        Ok((out, total))
    })
}

fn extract_backticked_tokens(line: &str) -> Vec<String> {
    let mut tokens = Vec::new();
    let mut rest = line;
    while let Some(open) = rest.find('`') {
        rest = &rest[open + 1..];
        let Some(close) = rest.find('`') else {
            break;
        };
        let tok = rest[..close].trim();
        if !tok.is_empty() && !tok.contains(' ') {
            tokens.push(tok.to_string());
        }
        rest = &rest[close + 1..];
    }
    tokens
}

fn looks_like_repo_path(tok: &str) -> bool {
    (tok.contains('/') && !tok.starts_with("http") && !tok.starts_with('/') && !tok.contains('*'))
        && matches!(
            Path::new(tok).extension().and_then(|e| e.to_str()),
            Some("rs" | "ts" | "tsx" | "js" | "py" | "go" | "md" | "toml" | "json")
        )
}

/// Native `flags`: detect feature/env/cfg gates across workspace source.
pub(super) fn native_flags(
    ctx: &ToolContext,
    target_path: &Path,
    filter: Option<&str>,
    limit: usize,
) -> Result<(String, usize), ToolError> {
    let root = tree_root_for(ctx, target_path);
    with_index(ctx, &root, |ws| {
        let scope = relative_scope(&root, target_path);
        let filter_low = filter.map(str::to_lowercase);
        let mut hits = Vec::new();
        for p in &ws.file_order {
            let Some(file) = ws.files_by_path.get(p) else {
                continue;
            };
            if !path_matches_scope(&file.rel_path, scope.as_deref()) {
                continue;
            }
            for (idx, line) in file.lines.iter().enumerate() {
                let trimmed = line.trim();
                let is_gate = trimmed.contains("#[cfg(")
                    || trimmed.contains("env::var")
                    || trimmed.contains("process.env.")
                    || trimmed.contains("os.environ");
                if !is_gate {
                    continue;
                }
                if let Some(ref f) = filter_low
                    && !trimmed.to_lowercase().contains(f)
                    && !file.rel_path.to_lowercase().contains(f)
                {
                    continue;
                }
                hits.push(format!("  {}:{} {}", file.rel_path, idx + 1, trimmed));
            }
        }
        let total = hits.len();
        let mut out = format!("Feature & env gates ({total} hits):\n");
        for row in hits.into_iter().take(limit) {
            out.push_str(&row);
            out.push('\n');
        }
        Ok((out, total))
    })
}

/// Native structural quality lens (`delta`, `dead_code`, `clones`, `hotspots`, `deps`, `seams`).
pub(super) fn native_quality(
    ctx: &ToolContext,
    target_path: &Path,
    mode: &str,
    limit: usize,
) -> Result<(String, usize), ToolError> {
    let root = tree_root_for(ctx, target_path);
    with_index(ctx, &root, |ws| {
        let scope = relative_scope(&root, target_path);
        let rows = match mode {
            "dead_code" => quality_dead_code(ws, scope.as_deref()),
            "hotspots" => quality_hotspots(ws, scope.as_deref(), limit),
            "clones" => quality_clones(ws, &root, scope.as_deref()),
            _ => quality_edges(ws, scope.as_deref(), limit),
        };
        let total = rows.len();
        let mut out = format!("Quality `{mode}` ({total} findings):\n");
        for row in rows.into_iter().take(limit) {
            out.push_str(&row);
            out.push('\n');
        }
        Ok((out, total))
    })
}

fn quality_dead_code(ws: &CachedWorkspace, scope: Option<&str>) -> Vec<String> {
    let mut rows = Vec::new();
    for sym in &ws.all_symbols {
        if !path_matches_scope(&sym.rel_path, scope) {
            continue;
        }
        if matches!(sym.sym.kind, SymbolKind::Function | SymbolKind::Method)
            && sym.sym.visibility == Visibility::Private
            && ws.fan_in(sym.id) == 0
            && sym.sym.name != "main"
            && !is_test_path_or_symbol(&sym.rel_path, &sym.sym.name)
        {
            rows.push(format!(
                "  {}:{} [private dead?] {}",
                sym.rel_path, sym.start_line, sym.sym.signature
            ));
        }
    }
    rows
}

fn quality_hotspots(ws: &CachedWorkspace, scope: Option<&str>, limit: usize) -> Vec<String> {
    let mut scored: Vec<(usize, &IndexedSymbol)> = ws
        .all_symbols
        .iter()
        .filter(|s| path_matches_scope(&s.rel_path, scope))
        .map(|s| {
            let span = s.end_line.saturating_sub(s.start_line) + 1;
            let deg = ws.fan_in(s.id) + ws.fan_out(s.id) + 1;
            (span * deg, s)
        })
        .collect();
    scored.sort_by_key(|a| std::cmp::Reverse(a.0));
    scored
        .into_iter()
        .take(limit)
        .map(|(score, s)| {
            format!(
                "  score={score:<4} {}:{} [{}] {}",
                s.rel_path,
                s.start_line,
                s.sym.kind.as_str(),
                s.sym.name
            )
        })
        .collect()
}

fn quality_clones(ws: &CachedWorkspace, root: &Path, scope: Option<&str>) -> Vec<String> {
    let mut by_norm: HashMap<String, Vec<String>> = HashMap::new();
    for sym in &ws.all_symbols {
        if !path_matches_scope(&sym.rel_path, scope)
            || sym.end_line.saturating_sub(sym.start_line) < 4
        {
            continue;
        }
        let abs = root.join(&sym.rel_path);
        let Some(file) = ws.files_by_path.get(&abs) else {
            continue;
        };
        let start = sym.start_line;
        let end = sym.end_line.min(file.lines.len());
        if start >= end {
            continue;
        }
        let body: String = file.lines[start..end]
            .iter()
            .map(|l| l.trim())
            .collect::<Vec<_>>()
            .join("\n");
        if body.len() >= 40 {
            by_norm.entry(body).or_default().push(format!(
                "{}:{} {}",
                sym.rel_path, sym.start_line, sym.sym.name
            ));
        }
    }
    by_norm
        .into_values()
        .filter(|locs| locs.len() >= 2)
        .map(|locs| format!("  duplicate body: {}", locs.join(" == ")))
        .collect()
}

fn quality_edges(ws: &CachedWorkspace, scope: Option<&str>, limit: usize) -> Vec<String> {
    let mut edges: BTreeMap<(String, String), usize> = BTreeMap::new();
    for (caller_id, callees) in ws.callees.iter().enumerate() {
        let from_mod = ws.all_symbols[caller_id].rel_path.clone();
        if !path_matches_scope(&from_mod, scope) {
            continue;
        }
        for &callee_id in callees {
            let to_mod = &ws.all_symbols[callee_id].rel_path;
            if &from_mod != to_mod {
                *edges.entry((from_mod.clone(), to_mod.clone())).or_default() += 1;
            }
        }
    }
    let mut sorted: Vec<((String, String), usize)> = edges.into_iter().collect();
    sorted.sort_by_key(|a| std::cmp::Reverse(a.1));
    sorted
        .into_iter()
        .take(limit)
        .map(|((a, b), count)| format!("  {a} -> {b} ({count} refs)"))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn test_ctx(dir: &Path) -> ToolContext {
        ToolContext {
            session_id: "test".to_string(),
            directory: dir.to_path_buf(),
            cwd: dir.to_path_buf(),
            cancel: None,
        }
    }

    #[test]
    fn native_index_covers_expand_impact_connect_communities_affected_trace() {
        let dir = TempDir::new().unwrap();
        let src = dir.path().join("src");
        let tests_dir = dir.path().join("tests");
        fs::create_dir_all(&src).unwrap();
        fs::create_dir_all(&tests_dir).unwrap();

        fs::write(
            src.join("core.rs"),
            "pub fn leaf_helper(x: i32) -> i32 {\n    x + 1\n}\n",
        )
        .unwrap();
        fs::write(
            src.join("service.rs"),
            "pub fn process_req(v: i32) -> i32 {\n    // comment\n    leaf_helper(v)\n}\n\npub fn entrypoint() -> i32 {\n    process_req(41)\n}\n",
        )
        .unwrap();
        fs::write(
            tests_dir.join("service_test.rs"),
            "#[test]\nfn test_entrypoint() {\n    let _ = entrypoint();\n}\n",
        )
        .unwrap();

        let ctx = test_ctx(dir.path());

        let (sym_out, sym_cnt) = native_symbol(&ctx, dir.path(), "process_req", 10).unwrap();
        assert_eq!(sym_cnt, 1);
        assert!(sym_out.contains("callers:[entrypoint"));
        assert!(sym_out.contains("calls:[leaf_helper"));

        let (exp_out, _) =
            native_expand(&ctx, dir.path(), "process_req", None, None, true).unwrap();
        assert!(exp_out.contains("leaf_helper(v)"));
        assert!(!exp_out.contains("// comment"));

        let (imp_out, imp_cnt) = native_impact(&ctx, dir.path(), "leaf_helper", 10).unwrap();
        assert!(imp_cnt >= 2);
        assert!(imp_out.contains("process_req"));
        assert!(imp_out.contains("entrypoint"));

        let (conn_out, hops) =
            native_connect(&ctx, dir.path(), "entrypoint", "leaf_helper").unwrap();
        assert!(hops >= 2);
        assert!(conn_out.contains("process_req"));

        let (comm_out, mods) = native_communities(&ctx, dir.path(), None, 10).unwrap();
        assert!(mods >= 2);
        assert!(comm_out.contains("src"));
        assert!(comm_out.contains("bridge"));

        let (aff_out, aff_cnt) = native_affected(&ctx, dir.path(), "leaf_helper", 10).unwrap();
        assert!(aff_cnt >= 1);
        assert!(aff_out.contains("test_entrypoint"));

        let (tr_out, tr_cnt) =
            native_trace(&ctx, dir.path(), "panicked at src/service.rs:3:5", 1200).unwrap();
        assert_eq!(tr_cnt, 1);
        assert!(tr_out.contains("process_req"));

        let (pack_out, pack_cnt) =
            native_pack_task(&ctx, dir.path(), "process_req helper", 1200, 10).unwrap();
        assert!(pack_cnt >= 1);
        assert!(pack_out.contains("<sigs>"));
        assert!(pack_out.contains("<bodies>"));
    }

    #[test]
    fn native_expand_respects_char_brace_literals_and_affected_avoids_suffix_collisions() {
        let dir = TempDir::new().unwrap();
        let src = dir.path().join("src");
        let tests_dir = dir.path().join("tests");
        fs::create_dir_all(&src).unwrap();
        fs::create_dir_all(&tests_dir).unwrap();

        fs::write(
            src.join("lib.rs"),
            "pub fn has_brace<'a>(_s: &'a str, c: char) -> bool {\n    let close = '}';\n    c == close\n}\n",
        )
        .unwrap();
        fs::write(
            src.join("other_lib.rs"),
            "pub fn unrelated_fn() -> i32 { 1 }\n",
        )
        .unwrap();
        fs::write(
            tests_dir.join("other_test.rs"),
            "#[test]\nfn test_unrelated() { let _ = unrelated_fn(); }\n",
        )
        .unwrap();

        let ctx = test_ctx(dir.path());
        let (exp_out, _) = native_expand(&ctx, dir.path(), "has_brace", None, None, false).unwrap();
        assert!(
            exp_out.contains("c == close"),
            "expand must not terminate early at char literal '}}': {exp_out}"
        );

        let affected = native_affected_tests_for_files(&ctx, dir.path(), &["lib.rs".to_string()]);
        assert!(
            affected.is_empty(),
            "lib.rs must not false-match tests referencing other_lib.rs: {affected:?}"
        );
    }

    #[test]
    fn native_cochange_ranks_git_partners_in_batch() {
        let dir = TempDir::new().unwrap();
        let run_git = |args: &[&str]| {
            let status = Command::new("git")
                .arg("-C")
                .arg(dir.path())
                .args(args)
                .status()
                .expect("git installed");
            assert!(status.success(), "git {args:?} failed");
        };
        run_git(&["init", "-q"]);
        run_git(&["config", "user.email", "test@example.com"]);
        run_git(&["config", "user.name", "Test"]);

        fs::create_dir_all(dir.path().join("src")).unwrap();
        fs::write(dir.path().join("src/a.rs"), "pub fn a() {}\n").unwrap();
        fs::write(dir.path().join("src/b.rs"), "pub fn b() {}\n").unwrap();
        run_git(&["add", "."]);
        run_git(&["commit", "-q", "-m", "init"]);

        fs::write(dir.path().join("src/a.rs"), "pub fn a() { /* v2 */ }\n").unwrap();
        fs::write(dir.path().join("src/b.rs"), "pub fn b() { /* v2 */ }\n").unwrap();
        run_git(&["add", "."]);
        run_git(&["commit", "-q", "-m", "coedit"]);

        let ctx = test_ctx(dir.path());
        let (report, count) = native_cochange(&ctx, &dir.path().join("src/a.rs"), 10);
        assert_eq!(count, 1, "unexpected report: {report}");
        assert!(
            report.contains("2x  src/b.rs"),
            "unexpected report: {report}"
        );
    }
}
