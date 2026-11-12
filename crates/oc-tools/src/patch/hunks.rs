//! Hunk model and the context matcher: exact, rstrip, trim and normalized comparison tiers.

use crate::util;

#[derive(Debug, Clone)]
pub struct Chunk {
    pub old_lines: Vec<String>,
    pub new_lines: Vec<String>,
    pub change_context: Option<String>,
    pub end_of_file: bool,
}

#[derive(Debug, Clone)]
pub enum Hunk {
    Add {
        path: String,
        contents: String,
    },
    Delete {
        path: String,
    },
    Update {
        path: String,
        move_path: Option<String>,
        chunks: Vec<Chunk>,
    },
}

/// Apply `chunks` to `original`, returning BOM-free content and the resulting
/// BOM flag.
pub fn derive(path: &str, chunks: &[Chunk], original: &str) -> Result<(String, bool), String> {
    let (source_bom, source) = util::split_bom(original);
    let mut lines: Vec<String> = source.split('\n').map(str::to_string).collect();
    if lines.last().is_some_and(String::is_empty) {
        lines.pop();
    }
    let replacements = compute_replacements(&lines, path, chunks)?;
    for (start, remove, insert) in replacements.into_iter().rev() {
        lines.splice(start..start + remove, insert);
    }
    if !lines.last().is_some_and(String::is_empty) {
        lines.push(String::new());
    }
    let joined = lines.join("\n");
    let (next_bom, next) = util::split_bom(&joined);
    Ok((next.to_string(), source_bom || next_bom))
}

fn compute_replacements(
    lines: &[String],
    path: &str,
    chunks: &[Chunk],
) -> Result<Vec<(usize, usize, Vec<String>)>, String> {
    let mut replacements: Vec<(usize, usize, Vec<String>)> = Vec::new();
    let mut line_index = 0usize;

    for chunk in chunks {
        if let Some(context) = &chunk.change_context {
            let found = seek(lines, std::slice::from_ref(context), line_index, false)
                .ok_or_else(|| format!("Failed to find context '{context}' in {path}"))?;
            line_index = found + 1;
        }
        if chunk.old_lines.is_empty() {
            replacements.push((lines.len(), 0, chunk.new_lines.clone()));
            continue;
        }

        let mut old: &[String] = &chunk.old_lines;
        let mut new: &[String] = &chunk.new_lines;
        let mut found = seek(lines, old, line_index, chunk.end_of_file);
        if found.is_none() && old.last().is_some_and(String::is_empty) {
            old = &old[..old.len() - 1];
            if new.last().is_some_and(String::is_empty) {
                new = &new[..new.len() - 1];
            }
            found = seek(lines, old, line_index, chunk.end_of_file);
        }
        let Some(found) = found else {
            if chunk.old_lines.iter().all(String::is_empty) {
                let expected = if chunk.old_lines.len() == 1 {
                    "an expected blank line".to_string()
                } else {
                    format!("{} consecutive blank lines", chunk.old_lines.len())
                };
                return Err(format!("Failed to find {expected} in {path}"));
            }
            return Err(format!(
                "Failed to find expected lines in {path}:\n{}",
                chunk.old_lines.join("\n")
            ));
        };
        replacements.push((found, old.len(), new.to_vec()));
        line_index = found + old.len();
    }

    replacements.sort_by_key(|replacement| replacement.0);
    Ok(replacements)
}

fn seek(lines: &[String], pattern: &[String], start: usize, eof: bool) -> Option<usize> {
    if pattern.is_empty() {
        return None;
    }
    let comparators: [fn(&str, &str) -> bool; 4] = [exact, rstrip, trim, normalized];

    if eof {
        if pattern.len() > lines.len() {
            return None;
        }
        let offset = lines.len() - pattern.len();
        if offset < start {
            return None;
        }
        for compare in comparators {
            if matches_at(lines, pattern, offset, compare) {
                return Some(offset);
            }
        }
        return None;
    }

    if pattern.len() > lines.len() {
        return None;
    }
    for compare in comparators {
        for offset in start..=lines.len() - pattern.len() {
            if matches_at(lines, pattern, offset, compare) {
                return Some(offset);
            }
        }
    }
    None
}

fn matches_at(
    lines: &[String],
    pattern: &[String],
    offset: usize,
    compare: fn(&str, &str) -> bool,
) -> bool {
    pattern
        .iter()
        .enumerate()
        .all(|(index, line)| compare(lines[offset + index].as_str(), line.as_str()))
}

fn exact(left: &str, right: &str) -> bool {
    left == right
}

fn rstrip(left: &str, right: &str) -> bool {
    left.trim_end() == right.trim_end()
}

fn trim(left: &str, right: &str) -> bool {
    left.trim() == right.trim()
}

fn normalized(left: &str, right: &str) -> bool {
    util::normalize_text(left.trim()) == util::normalize_text(right.trim())
}
