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

fn difference(before: &[&str], after: &[&str]) -> (Vec<&'static str>, usize, usize) {
    let width = after.len() + 1;
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
