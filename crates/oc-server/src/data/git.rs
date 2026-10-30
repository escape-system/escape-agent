//! Shared capture of one `git` invocation, used by the VCS and worktree runners.

/// Captured result of one `git` process.
#[derive(Debug)]
pub(crate) struct GitOutput {
    pub(crate) code: Option<i32>,
    pub(crate) stdout: Vec<u8>,
    pub(crate) stderr: String,
    /// True when the runner detected that the stdout cap cut the output short.
    pub(crate) truncated: bool,
}

impl GitOutput {
    pub(crate) fn ok(&self) -> bool {
        self.code == Some(0)
    }

    pub(crate) fn text(&self) -> String {
        String::from_utf8_lossy(&self.stdout).into_owned()
    }

    /// Best available diagnostic: stderr, then stdout.
    pub(crate) fn message(&self) -> String {
        let stderr = self.stderr.trim();
        if !stderr.is_empty() {
            return stderr.to_owned();
        }
        self.text().trim().to_owned()
    }
}
