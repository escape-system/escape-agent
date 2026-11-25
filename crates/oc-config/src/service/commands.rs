//! Global config location, slash commands, instruction files and the
//! per-directory instruction cache.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use crate::ConfigService;
use crate::commands::{self, CommandInfo};
use crate::discovery;
use crate::instructions::{self, InstructionFile};

use super::MAX_INSTRUCTION_CACHE;

impl ConfigService {
    /// The global configuration directory (`$XDG_CONFIG_HOME/opencode` or
    /// `~/.config/opencode`) discovered at load time.
    #[must_use]
    pub fn global_config_dir(&self) -> Option<PathBuf> {
        (!self.discovery.global.as_os_str().is_empty()).then(|| self.discovery.global.clone())
    }

    /// Markdown slash commands, sorted by name.
    #[must_use]
    pub fn commands(&self) -> &[CommandInfo] {
        &self.commands
    }

    #[must_use]
    pub fn command(&self, name: &str) -> Option<&CommandInfo> {
        self.commands.iter().find(|command| command.name == name)
    }

    /// Slash commands visible from `directory`, using the same global/project
    /// scoping as [`ConfigService::skills_for`].
    #[must_use]
    pub fn commands_for(&self, directory: &Path) -> Arc<Vec<CommandInfo>> {
        if directory.as_os_str().is_empty() {
            return Arc::new(self.commands.clone());
        }
        let resolved = if directory.is_absolute() {
            directory.to_path_buf()
        } else {
            self.discovery.location.join(directory)
        };
        let resolved = std::path::absolute(&resolved).unwrap_or(resolved);
        let key = discovery::path_key(&resolved);
        if key == discovery::path_key(&self.discovery.location) {
            return Arc::new(self.commands.clone());
        }
        if let Some(cached) = self.cached_commands(&key) {
            return cached;
        }

        let mut diagnostics = Vec::new();
        let (config_roots, _) = self.skill_roots();
        let found = Arc::new(commands::discover_for(
            &config_roots,
            &self.discovery,
            &resolved,
            &mut diagnostics,
        ));
        drop(diagnostics);
        self.cache_commands(key, Arc::clone(&found));
        found
    }

    /// Drop every cached per-directory command set, keeping the load-time
    /// snapshot.
    pub fn invalidate_commands(&self) {
        let mut cache = self
            .command_cache
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        cache.clear();
    }

    fn cached_commands(&self, key: &str) -> Option<Arc<Vec<CommandInfo>>> {
        let cache = self
            .command_cache
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        cache.get(key).cloned()
    }

    fn cache_commands(&self, key: String, commands: Arc<Vec<CommandInfo>>) {
        let mut cache = self
            .command_cache
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if cache.len() >= MAX_INSTRUCTION_CACHE && !cache.contains_key(&key) {
            cache.clear();
        }
        cache.insert(key, commands);
    }

    /// `AGENTS.md`/`CLAUDE.md`/`.opencode/AGENTS.md` from the location upward,
    /// outermost first.
    ///
    /// This is the load-time snapshot for the directory passed to
    /// [`ConfigService::load`]. Turns must call
    /// [`ConfigService::instructions_for`] with the session directory so a
    /// session in another project never inherits this snapshot.
    #[must_use]
    pub fn instructions(&self) -> &[InstructionFile] {
        self.instructions.as_slice()
    }

    /// Instruction files for `directory`, using the same upward walk and
    /// precedence as [`ConfigService::instructions`].
    ///
    /// Scoping is the point: a server started in one repository must not hand
    /// its `AGENTS.md` to sessions whose `location.directory` is another
    /// project. An empty or relative `directory` is resolved against the load
    /// directory (relative) or falls back to the load-time snapshot (empty), so
    /// the historical behavior is preserved when the session has no directory
    /// of its own.
    ///
    /// The result is cached per directory. Adding, removing or renaming an
    /// `AGENTS.md` after the first call is picked up only after
    /// [`ConfigService::invalidate_instructions`]; edited file contents are
    /// re-read by the runtime on every turn and are not stale.
    #[must_use]
    pub fn instructions_for(&self, directory: &Path) -> Arc<Vec<InstructionFile>> {
        if directory.as_os_str().is_empty() {
            return Arc::clone(&self.instructions);
        }
        let resolved = if directory.is_absolute() {
            directory.to_path_buf()
        } else {
            self.discovery.location.join(directory)
        };
        let resolved = std::path::absolute(&resolved).unwrap_or(resolved);
        let key = discovery::path_key(&resolved);
        if key == discovery::path_key(&self.discovery.location) {
            return Arc::clone(&self.instructions);
        }
        if let Some(cached) = self.cached_instructions(&key) {
            return cached;
        }

        let mut diagnostics = Vec::new();
        let files = Arc::new(instructions::discover(&resolved, &mut diagnostics));
        // Session-scoped diagnostics stay off the wire: `GET /api/config`
        // reports the load-time set, and a turn must not mutate it.
        drop(diagnostics);
        self.cache_instructions(key, Arc::clone(&files));
        files
    }

    /// Drop every cached per-directory instruction set, keeping the load-time
    /// snapshot. Call after the host reloads a location or writes an
    /// instruction file, so the next turn re-walks the filesystem.
    pub fn invalidate_instructions(&self) {
        let mut cache = self
            .instruction_cache
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        cache.clear();
    }

    fn cached_instructions(&self, key: &str) -> Option<Arc<Vec<InstructionFile>>> {
        let cache = self
            .instruction_cache
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        cache.get(key).cloned()
    }

    fn cache_instructions(&self, key: String, files: Arc<Vec<InstructionFile>>) {
        let mut cache = self
            .instruction_cache
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        // Bound the map: a long-lived server sees one entry per project
        // directory. Clearing is cheaper than an LRU and only costs a re-walk.
        if cache.len() >= MAX_INSTRUCTION_CACHE && !cache.contains_key(&key) {
            cache.clear();
        }
        cache.insert(key, files);
    }
}
