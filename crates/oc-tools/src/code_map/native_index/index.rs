use std::collections::{HashMap, HashSet};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex as StdMutex, OnceLock};
use std::time::{Duration, Instant, UNIX_EPOCH};

use super::super::Symbol;
use super::super::files::{collect_code_files, relative_to};
use super::parse::build_indexed_file;
use crate::{ToolContext, ToolError};

/// Maximum number of workspaces cached in the native graph index pool.
const MAX_CACHED_ROOTS: usize = 2;
/// Idle cached workspaces older than this are evicted on the next lookup.
const MAX_IDLE_AGE: Duration = Duration::from_secs(600);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct FileStamp {
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
pub(super) struct IndexedFile {
    pub(super) path: PathBuf,
    pub(super) rel_path: String,
    pub(super) ext: String,
    pub(super) stamp: FileStamp,
    pub(super) lines: Vec<String>,
    pub(super) symbols: Vec<IndexedSymbol>,
    pub(super) referenced_idents: Vec<Vec<String>>,
}

#[derive(Debug)]
pub(super) struct CachedWorkspace {
    pub(super) last_used: Instant,
    pub(super) files_by_path: HashMap<PathBuf, IndexedFile>,
    pub(super) file_order: Vec<PathBuf>,
    pub(super) all_symbols: Vec<IndexedSymbol>,
    pub(super) name_to_ids: HashMap<String, Vec<usize>>,
    pub(super) callees: Vec<Vec<usize>>,
    pub(super) callers: Vec<Vec<usize>>,
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

    pub(super) fn fan_in(&self, id: usize) -> usize {
        self.callers.get(id).map_or(0, Vec::len)
    }

    pub(super) fn fan_out(&self, id: usize) -> usize {
        self.callees.get(id).map_or(0, Vec::len)
    }

    pub(super) fn symbols_in_scope<'a>(
        &'a self,
        root: &Path,
        target_path: &Path,
    ) -> Vec<&'a IndexedSymbol> {
        let rel_scope = relative_scope(root, target_path);
        self.all_symbols
            .iter()
            .filter(|s| path_matches_scope(&s.rel_path, rel_scope.as_deref()))
            .collect()
    }

    pub(super) fn find_matching_symbols(
        &self,
        target_scope: Option<&str>,
        raw_query: &str,
    ) -> Vec<usize> {
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

    pub(super) fn find_by_file_line(&self, file_part: &str, line_num: usize) -> Vec<usize> {
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

pub(super) fn with_index<T>(
    ctx: &ToolContext,
    root: &Path,
    f: impl FnOnce(&CachedWorkspace) -> Result<T, ToolError>,
) -> Result<T, ToolError> {
    let canonical = fs::canonicalize(root).unwrap_or_else(|_| root.to_path_buf());
    let entry = {
        let mut guard = cache_pool()
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let stale: Vec<PathBuf> = guard
            .iter()
            .filter(|(k, ws)| {
                *k != &canonical
                    && ws
                        .try_lock()
                        .is_ok_and(|g| g.last_used.elapsed() > MAX_IDLE_AGE)
            })
            .map(|(k, _)| k.clone())
            .collect();
        for key in stale {
            guard.remove(&key);
        }
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

pub(in crate::code_map) fn relative_scope(root: &Path, target_path: &Path) -> Option<String> {
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

pub(super) fn path_matches_scope(rel_path: &str, scope: Option<&str>) -> bool {
    let Some(prefix) = scope else {
        return true;
    };
    rel_path == prefix || rel_path.starts_with(&format!("{prefix}/"))
}

pub(super) fn usize_to_f32(n: usize) -> f32 {
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
