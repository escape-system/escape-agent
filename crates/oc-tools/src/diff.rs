//! Minimal unified-diff metadata for `edit` and `patch`.
//!
//! The patch is intentionally simple (one hunk covering the file) but valid
//! unified diff text; counts come from a line LCS.

use std::fmt::Write as _;

use serde_json::{Value, json};

fn split(text: &str) -> Vec<&str> {
    if text.is_empty() {
        return Vec::new();
    }
    let mut lines: Vec<&str> = text.split('\n').collect();
    if text.ends_with('\n') {
        lines.pop();
    }
    lines
}

/// Hard cap on the LCS matrix cells (e.g. 2,000 x 2,000 lines) to prevent
/// quadratic memory blowup on massive files.
const MAX_DIFF_CELLS: usize = 4_000_000;

fn difference(before: &[&str], after: &[&str]) -> (Vec<&'static str>, usize, usize) {
    let width = after.len() + 1;
    let cells = (before.len() + 1).saturating_mul(width);
    if cells > MAX_DIFF_CELLS {
        let mut ops = Vec::with_capacity(before.len() + after.len());
        ops.resize(before.len(), "-");
        ops.resize(before.len() + after.len(), "+");
        return (ops, after.len(), before.len());
    }
    let mut table = vec![0u32; (before.len() + 1) * width];
    for i in (0..before.len()).rev() {
        for j in (0..after.len()).rev() {
            table[i * width + j] = if before[i] == after[j] {
                table[(i + 1) * width + j + 1] + 1
            } else {
                table[(i + 1) * width + j].max(table[i * width + j + 1])
            };
        }
    }
    let (mut i, mut j) = (0usize, 0usize);
    let mut ops = Vec::new();
    while i < before.len() && j < after.len() {
        if before[i] == after[j] {
            ops.push(" ");
            i += 1;
            j += 1;
        } else if table[(i + 1) * width + j] >= table[i * width + j + 1] {
            ops.push("-");
            i += 1;
        } else {
            ops.push("+");
            j += 1;
        }
    }
    while i < before.len() {
        ops.push("-");
        i += 1;
    }
    while j < after.len() {
        ops.push("+");
        j += 1;
    }
    let additions = ops.iter().filter(|op| **op == "+").count();
    let deletions = ops.iter().filter(|op| **op == "-").count();
    (ops, additions, deletions)
}

pub fn file_diff(file: &str, before: &str, after: &str, status: &str) -> Value {
    let before_lines = split(before);
    let after_lines = split(after);
    let (ops, additions, deletions) = difference(&before_lines, &after_lines);

    let mut patch = String::new();
    let _ = write!(patch, "--- {file}\n+++ {file}\n");
    let _ = writeln!(
        patch,
        "@@ -1,{} +1,{} @@",
        before_lines.len(),
        after_lines.len()
    );
    let (mut left, mut right) = (0usize, 0usize);
    for op in &ops {
        match *op {
            "-" => {
                patch.push('-');
                patch.push_str(before_lines[left]);
                patch.push('\n');
                left += 1;
            }
            "+" => {
                patch.push('+');
                patch.push_str(after_lines[right]);
                patch.push('\n');
                right += 1;
            }
            _ => {
                patch.push(' ');
                patch.push_str(before_lines[left]);
                patch.push('\n');
                left += 1;
                right += 1;
            }
        }
    }

    json!({
        "file": file,
        "patch": patch,
        "status": status,
        "additions": additions,
        "deletions": deletions,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn difference_caps_quadratic_growth() {
        let before: Vec<&str> = (0..2005).map(|_| "old").collect();
        let after: Vec<&str> = (0..2005).map(|_| "new").collect();
        let (ops, additions, deletions) = difference(&before, &after);
        assert_eq!(additions, 2005);
        assert_eq!(deletions, 2005);
        assert_eq!(ops.len(), 4010);
    }
}
