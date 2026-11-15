//! Construction: discovery, the loader run, the load-time snapshots and the
//! document view.

use std::collections::HashMap;
use std::sync::{Arc, RwLock};

use serde_json::{Map, Value};

use crate::discovery::{self, Env};
use crate::free::{
    build_agents, collect_skill_roots, default_shell, global_permissions, visible_skill_roots,
};
use crate::loader::Loader;
use crate::merge::merge_documents;
use crate::{
    ConfigEntry, ConfigError, ConfigService, LoadOptions, ModelsCell, commands, instructions,
    markdown_agents, normalize, permissions, skills,
};

impl ConfigService {
    pub fn load(options: LoadOptions) -> Result<Self, ConfigError> {
        let env = Env::new(&options.env);
        let directory = if options.directory.as_os_str().is_empty() {
            std::env::current_dir()?
        } else {
            options.directory
        };
        let discovery = discovery::discover(&directory, &env);

        let mut loader = Loader::new(&discovery, &env);
        loader.load_all();

        // End the loader's borrow of `discovery` before storing it.
        let Loader {
            entries,
            documents,
            mut diagnostics,
            ..
        } = loader;

        let mut permissions = global_permissions(&documents);
        permissions::expand_home(&mut permissions, &discovery.home);
        let (config_roots, extra_skill_roots) =
            collect_skill_roots(&entries, &documents, &discovery, &mut diagnostics);
        let skills = skills::discover(
            &config_roots,
            &extra_skill_roots,
            &discovery,
            &mut diagnostics,
        );
        let commands = commands::discover(&config_roots, &mut diagnostics);
        let instructions = Arc::new(instructions::discover(
            &discovery.location,
            &mut diagnostics,
        ));
        let markdown_agents =
            markdown_agents::discover(&config_roots, &discovery, &mut diagnostics);

        let merged = merge_documents(&documents);
        let (agents, default_agent) = build_agents(
            &documents,
            &permissions,
            &discovery.home,
            &markdown_agents,
            &mut diagnostics,
        );
        let skills_roots = visible_skill_roots(&config_roots, &extra_skill_roots);
        let mcp_servers = merged
            .get("mcp")
            .and_then(|mcp| mcp.get("servers"))
            .filter(|servers| servers.is_object())
            .cloned()
            .unwrap_or_else(|| Value::Object(Map::new()));
        let shell = merged
            .get("shell")
            .and_then(Value::as_str)
            .map_or_else(default_shell, str::to_owned);
        let model = merged.get("model").and_then(normalize::model_string);
        let models = merged
            .get("models")
            .filter(|value| value.is_object())
            .cloned()
            .unwrap_or_else(|| Value::Object(Map::new()));

        Ok(Self {
            entries,
            documents,
            discovery,
            merged,
            agents,
            default_agent,
            permissions,
            diagnostics,
            skills_roots,
            skills,
            commands,
            instructions,
            instruction_cache: RwLock::new(HashMap::new()),
            skill_cache: RwLock::new(HashMap::new()),
            mcp_servers,
            shell,
            model,
            models: RwLock::new(ModelsCell {
                value: models,
                revision: 0,
            }),
        })
    }

    /// Documents and directories for `GET /api/config`, lowest priority first.
    #[must_use]
    pub fn entries(&self) -> &[ConfigEntry] {
        &self.entries
    }

    /// Last-wins scalar merge with per-document folding of map-like keys.
    #[must_use]
    pub fn merged(&self) -> &Value {
        &self.merged
    }
}
