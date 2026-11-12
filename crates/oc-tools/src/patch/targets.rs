//! Authorization helper: every file path a patch text touches, in document order.

use std::collections::HashSet;

use super::parser::strip_heredoc;

/// Every file path a patch touches, in document order and deduplicated.
///
/// Both sides of an update-and-move are included (`*** Update File:` then
/// `*** Move to:`). This is an authorization helper: it reads only the patch
/// text and never touches the filesystem, and it tolerates malformed hunks by
/// extracting whatever headers are present.
#[must_use]
pub fn patch_targets(patch_text: &str) -> Vec<String> {
    let text = strip_heredoc(patch_text);
    let lines: Vec<&str> = text.lines().collect();
    let mut targets: Vec<String> = Vec::new();
    let mut seen: HashSet<String> = HashSet::new();
    let mut index = 0;
    while index < lines.len() {
        let header = lines[index].trim();
        if let Some(path) = header.strip_prefix("*** Add File: ") {
            push_target(&mut targets, &mut seen, path);
        } else if let Some(path) = header.strip_prefix("*** Delete File: ") {
            push_target(&mut targets, &mut seen, path);
        } else if let Some(path) = header.strip_prefix("*** Update File: ") {
            push_target(&mut targets, &mut seen, path);
            let mut next = index + 1;
            while next < lines.len() && lines[next].trim_end() == "*** End of File" {
                next += 1;
            }
            if let Some(candidate) = lines.get(next) {
                let candidate = candidate.trim_end();
                if let Some(destination) = candidate.strip_prefix("*** Move to:")
                    && (destination.is_empty() || destination.starts_with(' '))
                {
                    push_target(&mut targets, &mut seen, destination);
                    index = next;
                }
            }
        }
        index += 1;
    }
    targets
}

fn push_target(targets: &mut Vec<String>, seen: &mut HashSet<String>, path: &str) {
    let path = path.trim();
    if path.is_empty() {
        return;
    }
    if seen.insert(path.to_string()) {
        targets.push(path.to_string());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn patch_targets_extracts_every_hunk_kind() {
        let patch = "*** Begin Patch\n\
                     *** Add File: added.txt\n\
                     +hello\n\
                     *** Delete File: removed.txt\n\
                     *** Update File: updated.txt\n\
                     @@\n\
                     -old\n\
                     +new\n\
                     *** End Patch\n";
        assert_eq!(
            patch_targets(patch),
            vec!["added.txt", "removed.txt", "updated.txt"]
        );
    }

    #[test]
    fn patch_targets_includes_both_sides_of_a_move() {
        let patch = "*** Begin Patch\n\
                     *** Update File: old/name.txt\n\
                     *** Move to: new/name.txt\n\
                     @@\n\
                     -a\n\
                     +b\n\
                     *** Update File: later.txt\n\
                     @@\n\
                     -c\n\
                     +d\n\
                     *** End Patch\n";
        assert_eq!(
            patch_targets(patch),
            vec!["old/name.txt", "new/name.txt", "later.txt"]
        );
    }

    #[test]
    fn patch_targets_dedupes_and_preserves_document_order() {
        let patch = "*** Begin Patch\n\
                     *** Update File: a.txt\n\
                     @@\n\
                     -1\n\
                     +2\n\
                     *** Update File: b.txt\n\
                     @@\n\
                     -1\n\
                     +2\n\
                     *** Update File: a.txt\n\
                     @@\n\
                     -2\n\
                     +3\n\
                     *** Delete File: b.txt\n\
                     *** End Patch\n";
        assert_eq!(patch_targets(patch), vec!["a.txt", "b.txt"]);
    }

    #[test]
    fn patch_targets_handles_environment_header_and_heredoc() {
        let patch = "cat <<'EOF'\n*** Begin Patch\n*** Environment ID: env-1\n*** Add File: only.txt\n+x\n*** End Patch\nEOF";
        assert_eq!(patch_targets(patch), vec!["only.txt"]);
    }

    #[test]
    fn patch_targets_skips_empty_and_space_less_move_headers() {
        let patch = "*** Begin Patch\n*** Update File: \n*** Move to:\n*** End Patch\n";
        assert!(patch_targets(patch).is_empty());

        let no_space =
            "*** Begin Patch\n*** Update File: a.txt\n*** Move to:b.txt\n*** End Patch\n";
        assert_eq!(patch_targets(no_space), vec!["a.txt"]);
    }
}
