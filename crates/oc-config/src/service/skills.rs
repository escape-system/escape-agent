//! Skill discovery, the per-directory skill cache and its invalidation.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use crate::ConfigService;
use crate::discovery;
use crate::free::collect_skill_roots;
use crate::skills::{self, SkillInfo};

use super::MAX_INSTRUCTION_CACHE;

impl ConfigService {
    /// Configuration roots that contribute `skill/` and `skills/`, in load
    /// order. Local `skills[]` entries are resolved; URL entries are skipped
    /// because loading never touches the network.
    #[must_use]
    pub fn skills_roots(&self) -> Vec<PathBuf> {
        self.skills_roots.clone()
    }

    /// Discovered skills, sorted by id. Later roots and files win on id
    /// collisions; URL bundles are skipped (no network) with a diagnostic.
    #[must_use]
    pub fn skills(&self) -> &[SkillInfo] {
        &self.skills
    }

    #[must_use]
    pub fn skill(&self, id: &str) -> Option<&SkillInfo> {
        self.skills.iter().find(|skill| skill.id == id)
    }

    /// Re-run skill discovery over the same roots and return the sorted
    /// result. Use after writing a `SKILL.md`; the service itself keeps its
    /// load-time snapshot.
    #[must_use]
    pub fn rescan_skills(&self) -> Vec<SkillInfo> {
        let mut diagnostics = Vec::new();
        let (config_roots, extra_roots) = self.skill_roots();
        skills::discover(
            &config_roots,
            &extra_roots,
            &self.discovery,
            &mut diagnostics,
        )
    }

    /// Skills visible from `directory`, using the same roots and precedence as
    /// [`ConfigService::skills`].
    ///
    /// Scoping is the point: a server started in one repository must not hand
    /// its `.opencode/skill/*` to sessions whose `location.directory` is another
    /// project. Project-scope roots outside the session tree are dropped and
    /// the session tree's own `.opencode` directories are scanned, so the index
    /// matches the project the session actually works in. Global configuration
    /// roots, configured `skills[]` directories and the per-user
    /// `.claude`/`.agents` roots stay visible: they are not project-scoped.
    ///
    /// An empty `directory` returns the load-time snapshot. A relative path is
    /// resolved against the load directory. The result is cached per directory
    /// until [`ConfigService::invalidate_skills`]; a skill file written after
    /// the first call is picked up only after invalidation (skill ids already
    /// loaded in a session are unaffected, since the index is frozen into the
    /// instruction prefix).
    #[must_use]
    pub fn skills_for(&self, directory: &Path) -> Arc<Vec<SkillInfo>> {
        if directory.as_os_str().is_empty() {
            return Arc::new(self.skills.clone());
        }
        let resolved = if directory.is_absolute() {
            directory.to_path_buf()
        } else {
            self.discovery.location.join(directory)
        };
        let resolved = std::path::absolute(&resolved).unwrap_or(resolved);
        let key = discovery::path_key(&resolved);
        if key == discovery::path_key(&self.discovery.location) {
            return Arc::new(self.skills.clone());
        }
        if let Some(cached) = self.cached_skills(&key) {
            return cached;
        }

        let mut diagnostics = Vec::new();
        let (config_roots, extra_roots) = self.skill_roots();
        let found = Arc::new(skills::discover_for(
            &config_roots,
            &extra_roots,
            &self.discovery,
            &resolved,
            &mut diagnostics,
        ));
        // Session-scoped diagnostics stay off the wire, like instructions_for.
        drop(diagnostics);
        self.cache_skills(key, Arc::clone(&found));
        found
    }

    /// Drop every cached per-directory skill set, keeping the load-time
    /// snapshot. Call after writing a `SKILL.md` so the next turn's index sees
    /// it.
    pub fn invalidate_skills(&self) {
        let mut cache = self
            .skill_cache
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        cache.clear();
    }

    pub(super) fn skill_roots(&self) -> (Vec<PathBuf>, Vec<PathBuf>) {
        let mut diagnostics = Vec::new();
        collect_skill_roots(
            &self.entries,
            &self.documents,
            &self.discovery,
            &mut diagnostics,
        )
    }

    fn cached_skills(&self, key: &str) -> Option<Arc<Vec<SkillInfo>>> {
        let cache = self
            .skill_cache
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        cache.get(key).cloned()
    }

    fn cache_skills(&self, key: String, skills: Arc<Vec<SkillInfo>>) {
        let mut cache = self
            .skill_cache
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        // Same bound as the instruction cache: clear rather than evict one.
        if cache.len() >= MAX_INSTRUCTION_CACHE && !cache.contains_key(&key) {
            cache.clear();
        }
        cache.insert(key, skills);
    }
}
