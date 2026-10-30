//! File and diff wire entities (`FileSystem.Entry`, `FileDiff.Info`).

use serde::{Deserialize, Serialize};

/// `FileSystem.Entry.type`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum FileKind {
    File,
    Directory,
}

/// `FileSystem.Entry`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FileEntry {
    pub path: String,
    #[serde(rename = "type")]
    pub kind: FileKind,
}

/// `FileDiff.Info.status`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum FileStatus {
    Added,
    Deleted,
    Modified,
}

/// `FileDiff.Info`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FileDiff {
    pub file: String,
    pub patch: String,
    pub additions: i64,
    pub deletions: i64,
    pub status: FileStatus,
    /// Literal `true` on the truncation marker entry.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub more: Option<bool>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn file_entry_uses_type_discriminator() {
        let entry = FileEntry {
            path: "src/main.rs".to_owned(),
            kind: FileKind::File,
        };
        assert_eq!(
            serde_json::to_value(&entry).unwrap(),
            serde_json::json!({"path": "src/main.rs", "type": "file"})
        );
    }

    #[test]
    fn diff_omits_more_when_absent() {
        let diff = FileDiff {
            file: "a.rs".to_owned(),
            patch: "@@ -1 +1 @@".to_owned(),
            additions: 1,
            deletions: 0,
            status: FileStatus::Modified,
            more: None,
        };
        let text = serde_json::to_string(&diff).unwrap();
        assert_eq!(
            text,
            r#"{"file":"a.rs","patch":"@@ -1 +1 @@","additions":1,"deletions":0,"status":"modified"}"#
        );
    }
}
