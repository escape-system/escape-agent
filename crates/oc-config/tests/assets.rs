//! Tests for skill, command, markdown-agent and instruction discovery.
//!
//! Every fixture is isolated under its own temporary directory. The
//! instruction walk legitimately reaches the filesystem root, so assertions
//! filter to paths inside the fixture.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use oc_config::{ConfigService, Effect, LoadOptions, SkillSource};

struct Fixture {
    root: tempfile::TempDir,
}

impl Fixture {
    fn new() -> Self {
        Self {
            root: tempfile::tempdir().expect("temp dir"),
        }
    }

    fn path(&self, relative: &str) -> PathBuf {
        self.root.path().join(relative)
    }

    fn write(&self, relative: &str, content: &str) -> PathBuf {
        let path = self.path(relative);
        std::fs::create_dir_all(path.parent().expect("parent")).expect("mkdir");
        std::fs::write(&path, content).expect("write");
        path
    }

    fn make_dir(&self, relative: &str) -> PathBuf {
        let path = self.path(relative);
        std::fs::create_dir_all(&path).expect("mkdir");
        path
    }

    fn options(&self, directory: &Path) -> LoadOptions {
        let mut env = HashMap::new();
        env.insert(
            "HOME".to_owned(),
            self.path("home").to_string_lossy().into_owned(),
        );
        env.insert(
            "XDG_CONFIG_HOME".to_owned(),
            self.path("xdg").to_string_lossy().into_owned(),
        );
        env.insert(
            "ProgramData".to_owned(),
            self.path("programdata").to_string_lossy().into_owned(),
        );
        env.insert(
            "OPENCODE_CONFIG".to_owned(),
            self.path("missing-explicit.json")
                .to_string_lossy()
                .into_owned(),
        );
        // Shadow runner-exported inline config so a real session cannot inject
        // documents into fixtures. Whitespace survives the "empty means unset"
        // rule and is skipped by loading.
        env.insert("OPENCODE_CONFIG_CONTENT".to_owned(), " ".to_owned());
        LoadOptions {
            directory: directory.to_path_buf(),
            env,
        }
    }

    fn load(&self, directory: &Path) -> ConfigService {
        ConfigService::load(self.options(directory)).expect("load")
    }
}

fn skill_file(frontmatter: &str, body: &str) -> String {
    format!("---\n{frontmatter}\n---\n{body}")
}

/// Point `link` at `target` as a directory link. Unix uses a symlink; Windows
/// uses a junction so no elevation is required.
#[cfg(unix)]
fn link_directory(target: &Path, link: &Path) {
    std::os::unix::fs::symlink(target, link).expect("symlink");
}

#[cfg(windows)]
fn link_directory(target: &Path, link: &Path) {
    let output = std::process::Command::new("cmd")
        .args(["/C", "mklink", "/J"])
        .arg(link)
        .arg(target)
        .output()
        .expect("mklink");
    assert!(
        output.status.success(),
        "junction failed: {}\nlink={} target={}",
        String::from_utf8_lossy(&output.stderr),
        link.display(),
        target.display()
    );
    assert!(link.is_dir());
}

#[test]
#[allow(clippy::too_many_lines)]
fn discovers_skills_with_precedence_and_compatibility_roots() {
    let fixture = Fixture::new();
    let global = "xdg/opencode";
    let project_dir = fixture.make_dir("project");
    let project = project_dir.to_string_lossy().into_owned();

    fixture.write(
        "xdg/opencode/opencode.json",
        r#"{"skills": ["https://example.com/bundle", "shared-skills"]}"#,
    );

    // Global config root: directory skill, loose markdown, nested directory.
    fixture.write(
        &format!("{global}/skill/alpha/SKILL.md"),
        &skill_file(
            "name: Alpha Skill\ndescription: first\nmetadata:\n  opencode/autoinvoke: false",
            "alpha body",
        ),
    );
    fixture.write(
        &format!("{global}/skill/loose.md"),
        &skill_file("name: Loose Skill", "loose body"),
    );
    fixture.write(
        &format!("{global}/skill/outer/inner/SKILL.md"),
        &skill_file("name: Inner", "inner body"),
    );
    // Missing `name`: id fallback plus diagnostic.
    fixture.write(
        &format!("{global}/skill/unnamed/SKILL.md"),
        &skill_file("description: no name", "unnamed body"),
    );
    // disable-model-invocation maps to autoinvoke=false.
    fixture.write(
        &format!("{global}/skill/no-model/SKILL.md"),
        &skill_file("name: No Model\ndisable-model-invocation: yes", "body"),
    );
    // Dot and heavy directories are skipped.
    fixture.write(
        &format!("{global}/skill/.hidden/SKILL.md"),
        &skill_file("name: Hidden", "hidden"),
    );
    fixture.write(
        &format!("{global}/skill/node_modules/pkg/SKILL.md"),
        &skill_file("name: Module", "module"),
    );
    // Depth boundary: seven directory levels are reachable, the eighth is not.
    fixture.write(
        &format!("{global}/skill/a/b/c/d/e/f/g/SKILL.md"),
        &skill_file("name: Deep", "deep"),
    );
    fixture.write(
        &format!("{global}/skill/a/b/c/d/e/f/g/h/SKILL.md"),
        &skill_file("name: Too Deep", "too deep"),
    );

    fixture.write(
        &format!("{project}/shared-skills/direct-skill/SKILL.md"),
        &skill_file("name: Direct Skill", "direct body"),
    );

    // Project root wins on id collisions.
    fixture.write(
        &format!("{project}/.opencode/skill/dupe/SKILL.md"),
        &skill_file("name: Project Dupe", "project body"),
    );
    fixture.write(
        &format!("{global}/skill/dupe/SKILL.md"),
        &skill_file("name: Global Dupe", "global body"),
    );

    // Compatibility roots.
    fixture.write(
        "home/.claude/skills/claude-one/SKILL.md",
        &skill_file("name: Claude One", "claude"),
    );
    fixture.write(
        "home/.agents/skills/agents-one/SKILL.md",
        &skill_file("name: Agents One", "agents"),
    );
    fixture.write(
        &format!("{project}/.claude/skills/project-claude/SKILL.md"),
        &skill_file("name: Project Claude", "project claude"),
    );

    let service = fixture.load(&project_dir);

    let alpha = service.skill("alpha").expect("alpha");
    assert_eq!(alpha.name, "Alpha Skill");
    assert_eq!(alpha.description.as_deref(), Some("first"));
    assert_eq!(alpha.autoinvoke, Some(false));
    assert_eq!(alpha.content, "alpha body");
    assert_eq!(alpha.source, SkillSource::Config);

    let loose = service.skill("loose").expect("loose");
    assert_eq!(loose.name, "Loose Skill");
    assert_eq!(loose.autoinvoke, None);

    assert_eq!(service.skill("inner").expect("inner").content, "inner body");
    assert_eq!(
        service.skill("direct-skill").expect("direct-skill").content,
        "direct body",
        "skills[] directories are scanned"
    );
    assert_eq!(
        service.skill("unnamed").expect("unnamed").name,
        "unnamed",
        "missing name falls back to the id"
    );
    assert_eq!(
        service.skill("no-model").expect("no-model").autoinvoke,
        Some(false)
    );
    assert!(service.skill("hidden").is_none());
    assert!(service.skill("pkg").is_none());
    assert!(service.skill("Module").is_none());
    assert!(service.skill("g").is_some(), "depth 6 is included");
    assert!(service.skill("h").is_none(), "depth 7 is beyond the limit");

    let dupe = service.skill("dupe").expect("dupe");
    assert_eq!(dupe.name, "Project Dupe");
    assert_eq!(dupe.content, "project body");
    assert_eq!(dupe.source, SkillSource::Project);

    assert_eq!(
        service.skill("claude-one").expect("claude").source,
        SkillSource::Claude
    );
    assert_eq!(
        service.skill("agents-one").expect("agents").source,
        SkillSource::Agents
    );
    assert_eq!(
        service
            .skill("project-claude")
            .expect("project claude")
            .source,
        SkillSource::Claude
    );

    let ours: Vec<&str> = service
        .skills()
        .iter()
        .filter(|skill| {
            [
                "alpha",
                "loose",
                "inner",
                "unnamed",
                "no-model",
                "dupe",
                "claude-one",
                "agents-one",
                "project-claude",
            ]
            .contains(&skill.id.as_str())
        })
        .map(|skill| skill.id.as_str())
        .collect();
    let mut sorted = ours.clone();
    sorted.sort_unstable();
    assert_eq!(ours, sorted, "skills are sorted by id");

    assert!(
        service
            .diagnostics()
            .iter()
            .any(|diagnostic| diagnostic.message.contains("skipped URL skill bundle")),
        "URL skill bundles are skipped with a diagnostic"
    );
    assert!(
        service
            .diagnostics()
            .iter()
            .any(|diagnostic| diagnostic.message.contains("duplicate skill id `dupe`")),
        "duplicate ids are reported and the later one wins"
    );
    assert!(
        service
            .diagnostics()
            .iter()
            .any(|diagnostic| diagnostic.message.contains("has no frontmatter `name`"))
    );
}

#[test]
fn discovers_commands_with_frontmatter_and_precedence() {
    let fixture = Fixture::new();
    let global = "xdg/opencode";
    let project_dir = fixture.make_dir("project");
    let project = project_dir.to_string_lossy().into_owned();

    fixture.write(
        &format!("{global}/command/hello.md"),
        &skill_file("description: global hello", "global hello body"),
    );
    fixture.write(
        &format!("{project}/.opencode/command/hello.md"),
        &skill_file(
            "description: project hello\nagent: build\nmodel: anthropic/claude-sonnet-4-5\nsubtask: yes",
            "project hello body",
        ),
    );
    fixture.write(
        &format!("{project}/.opencode/command/nested/deep.md"),
        &skill_file("description: deep", "deep body"),
    );
    fixture.write(
        &format!("{global}/commands/plain.md"),
        "No frontmatter here",
    );
    fixture.write(
        &format!("{project}/.opencode/commands/aliased.md"),
        &skill_file("subagent: true", "alias body"),
    );

    let service = fixture.load(&project_dir);

    let hello = service.command("hello").expect("hello");
    assert_eq!(hello.description.as_deref(), Some("project hello"));
    assert_eq!(hello.agent.as_deref(), Some("build"));
    assert_eq!(hello.model.as_deref(), Some("anthropic/claude-sonnet-4-5"));
    assert_eq!(hello.subtask, Some(true));
    assert_eq!(hello.template, "project hello body");
    assert!(service.diagnostics().iter().any(|diagnostic| {
        diagnostic
            .message
            .contains("duplicate command name `hello`")
    }));

    assert_eq!(service.command("deep").expect("deep").template, "deep body");
    let plain = service.command("plain").expect("plain");
    assert_eq!(plain.template, "No frontmatter here");
    assert_eq!(plain.description, None);
    assert_eq!(
        service.command("aliased").expect("aliased").subtask,
        Some(true)
    );

    let ours: Vec<&str> = service
        .commands()
        .iter()
        .filter(|command| ["hello", "deep", "plain", "aliased"].contains(&command.name.as_str()))
        .map(|command| command.name.as_str())
        .collect();
    let mut sorted = ours.clone();
    sorted.sort_unstable();
    assert_eq!(ours, sorted, "commands are sorted by name");
}

#[test]
fn markdown_agents_merge_after_json_and_inherit_permissions() {
    let fixture = Fixture::new();
    let global = "xdg/opencode";
    let project = fixture.make_dir("project");

    fixture.write(
        &format!("{global}/opencode.json"),
        r#"{
            "permissions": [{"action": "edit", "resource": "docs/*", "effect": "deny"}],
            "agents": {
                "reviewer": {"description": "from json", "model": "openai/gpt-5"},
                "retired": {"description": "json only"}
            }
        }"#,
    );
    fixture.write(
        &format!("{global}/agent/reviewer.md"),
        &skill_file(
            "description: from markdown\nmodel: anthropic/claude-sonnet-4-5",
            "You review.",
        ),
    );
    fixture.write(
        &format!("{global}/agent/helper.md"),
        &skill_file("description: helper\nmode: subagent", "Help."),
    );
    fixture.write(
        &format!("{global}/agent/defaulted.md"),
        &skill_file("description: defaulted", "Default."),
    );
    fixture.write(
        &format!("{global}/modes/quick.md"),
        &skill_file("description: quick\nmode: subagent", "Quick."),
    );
    fixture.write(
        &format!("{global}/agent/retired.md"),
        &skill_file("disabled: true", ""),
    );

    let service = fixture.load(&project);

    let reviewer = service.agent("reviewer").expect("reviewer");
    assert_eq!(reviewer["description"], "from markdown");
    assert_eq!(reviewer["system"], "You review.");
    assert_eq!(reviewer["model"]["id"], "claude-sonnet-4-5");
    assert_eq!(reviewer["model"]["providerID"], "anthropic");

    let helper = service.agent("helper").expect("helper");
    assert_eq!(helper["mode"], "subagent");
    assert_eq!(helper["system"], "Help.");
    assert_eq!(
        service.evaluate(Some("helper"), "edit", "docs/readme.md"),
        Effect::Deny,
        "markdown agents inherit document permissions"
    );

    assert_eq!(
        service.agent("defaulted").expect("defaulted")["mode"],
        "all",
        "markdown agents default to mode all"
    );
    assert_eq!(
        service.agent("quick").expect("quick")["mode"],
        "primary",
        "mode directories force primary"
    );
    assert!(
        service.agent("retired").is_none(),
        "disabled removes the agent"
    );
    assert!(service.agent("build").is_some(), "built-ins survive");
}

#[test]
fn instructions_walk_upward_outermost_first_and_dedupe() {
    let fixture = Fixture::new();
    fixture.make_dir("project");
    let nested = fixture.make_dir("project/sub");

    fixture.write("project/AGENTS.md", "outer");
    fixture.write("project/sub/AGENTS.md", "inner");
    fixture.write("project/sub/CLAUDE.md", "claude");

    // The same file reached through `.opencode/AGENTS.md` must appear once.
    link_directory(
        &fixture.path("project"),
        &fixture.path("project").join(".opencode"),
    );

    let service = fixture.load(&nested);
    let ours: Vec<_> = service
        .instructions()
        .iter()
        .filter(|file| file.path.starts_with(fixture.root.path()))
        .collect();

    assert_eq!(
        ours.iter()
            .map(|file| file.text.as_str())
            .collect::<Vec<_>>(),
        ["outer", "inner", "claude"],
        "outermost first, per-directory order AGENTS.md then CLAUDE.md"
    );
    assert!(
        ours[0].path.ends_with("project/AGENTS.md"),
        "the first path found wins the dedupe"
    );
}

#[test]
fn instructions_skip_oversized_files_and_missing_directories() {
    let fixture = Fixture::new();
    let empty = fixture.make_dir("project/empty");
    let service = fixture.load(&empty);
    assert!(
        service
            .instructions()
            .iter()
            .all(|file| !file.path.starts_with(fixture.root.path())),
        "no instruction files when none exist"
    );

    let oversized = "x".repeat(1024 * 1024 + 1);
    fixture.write("project/AGENTS.md", &oversized);
    let service = fixture.load(&fixture.path("project"));
    assert!(
        service
            .instructions()
            .iter()
            .all(|file| !file.path.ends_with("project/AGENTS.md")),
        "files above 1 MiB are skipped"
    );
    assert!(
        service
            .diagnostics()
            .iter()
            .any(|diagnostic| diagnostic.message.contains("1 MiB cap")),
        "the skip is reported"
    );
}

#[test]
fn rescan_skills_sees_files_written_after_load() {
    let fixture = Fixture::new();
    let project = fixture.make_dir("project/.opencode");
    fixture.write(
        "xdg/opencode/skill/before/SKILL.md",
        &skill_file("name: Before\ndescription: loaded", "before body"),
    );
    let service = fixture.load(&project);

    assert!(service.skill("after").is_none());
    fixture.write(
        "project/.opencode/skill/after/SKILL.md",
        &skill_file("name: After\ndescription: fresh", "after body"),
    );

    let skills = service.rescan_skills();
    let after = skills
        .iter()
        .find(|skill| skill.id == "after")
        .expect("rescan sees the new file");
    assert_eq!(after.content, "after body");
    assert_eq!(after.source, SkillSource::Project);
    let ids: Vec<&str> = skills.iter().map(|skill| skill.id.as_str()).collect();
    let mut sorted = ids.clone();
    sorted.sort_unstable();
    assert_eq!(ids, sorted, "rescan keeps the sorted order");
    assert!(
        service.skill("after").is_none(),
        "the load-time snapshot is unchanged"
    );
}

#[test]
fn rescan_marks_agent_authored_skills() {
    let fixture = Fixture::new();
    let project = fixture.make_dir("project/.opencode");
    fixture.write(
        "project/.opencode/skill/written-by-agent/SKILL.md",
        &skill_file(
            "name: Agent Skill\ndescription: authored\nmetadata:\n  opencode/agent: true",
            "agent body",
        ),
    );
    let service = fixture.load(&project);
    let skill = service.skill("written-by-agent").expect("skill");
    assert_eq!(skill.source, SkillSource::Agent);
    assert_eq!(
        service
            .rescan_skills()
            .into_iter()
            .find(|skill| skill.id == "written-by-agent")
            .expect("rescan")
            .source,
        SkillSource::Agent
    );
}

#[test]
fn global_config_dir_is_the_discovered_global_root() {
    let fixture = Fixture::new();
    let project = fixture.make_dir("project");
    let service = fixture.load(&project);
    let global = service.global_config_dir().expect("global config dir");
    assert_eq!(global, fixture.path("xdg/opencode"));
}
