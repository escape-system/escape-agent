use std::collections::HashMap;
use std::path::Path;

use oc_config::{ConfigService, LoadOptions, parse_markdown};
use oc_types::EventPayload;
use serde_json::json;

use super::activate::{SkillActivated, find_skill};
use super::lint::skill_id_from_path;
use super::write::{compose_document, validate_id};

#[test]
fn ids_follow_the_documented_pattern() {
    assert!(validate_id("rust-build-test").is_ok());
    assert!(validate_id("a1").is_ok());
    assert!(validate_id("A1").is_err());
    assert!(validate_id("a").is_err(), "two characters minimum");
    assert!(validate_id("-a").is_err());
    assert!(validate_id("a_b").is_err());
    assert!(validate_id(&format!("a{}", "b".repeat(64))).is_err());
}

#[test]
fn documents_carry_the_agent_marker_and_quoted_scalars() {
    let document = compose_document(
        "Rust: Build",
        "Use lanes; never raw cargo test",
        Some(true),
        "Body\n",
    );
    assert!(document.starts_with("---\nname: \"Rust: Build\"\n"));
    assert!(
        document
            .contains("\nmetadata:\n  opencode/agent: true\n  opencode/autoinvoke: true\n---\n")
    );
    let parsed = parse_markdown(&document);
    assert!(parsed.error.is_none());
    assert_eq!(parsed.body.trim(), "Body");
}

#[test]
fn activation_event_is_durable_v1_with_the_documented_fields() {
    let payload = SkillActivated {
        session_id: "ses_one",
        id: "rust-build-test",
        name: "Rust Build",
        text: "body",
    };
    assert_eq!(
        <SkillActivated as EventPayload>::TYPE,
        "session.skill.activated"
    );
    const { assert!(<SkillActivated as EventPayload>::DURABLE) };
    assert_eq!(<SkillActivated as EventPayload>::VERSION, 1);
    assert_eq!(
        payload.data(),
        json!({
            "sessionID": "ses_one",
            "id": "rust-build-test",
            "name": "Rust Build",
            "text": "body",
        })
    );
}

#[test]
fn skill_id_is_derived_like_discovery() {
    assert_eq!(
        skill_id_from_path(Path::new("/x/skill/rust-build-test/SKILL.md")).as_deref(),
        Some("rust-build-test")
    );
    assert_eq!(
        skill_id_from_path(Path::new("/x/skill/loose.md")).as_deref(),
        Some("loose")
    );
}

/// One temp tree: a `server` project the config was loaded for, a `session`
/// project the tool runs in, and an isolated XDG config root.
fn scoped_config(root: &Path) -> ConfigService {
    let mut env = HashMap::new();
    env.insert(
        "HOME".to_owned(),
        root.join("home").to_string_lossy().into_owned(),
    );
    env.insert(
        "XDG_CONFIG_HOME".to_owned(),
        root.join("xdg").to_string_lossy().into_owned(),
    );
    env.insert(
        "OPENCODE_CONFIG".to_owned(),
        root.join("missing.json").to_string_lossy().into_owned(),
    );
    // Shadow runner-exported inline config so a real session cannot inject
    // documents into the fixture; loading skips whitespace.
    env.insert("OPENCODE_CONFIG_CONTENT".to_owned(), " ".to_owned());
    let options = LoadOptions {
        directory: root.join("server"),
        env,
    };
    ConfigService::load(options).expect("load")
}

fn write_skill(root: &Path, relative: &str, id: &str) {
    let path = root.join(relative).join("skill").join(id).join("SKILL.md");
    std::fs::create_dir_all(path.parent().expect("parent")).expect("mkdir");
    std::fs::write(
        &path,
        format!("---\nname: {id}\ndescription: fixture\n---\n{id} body\n"),
    )
    .expect("write");
}

/// The loader must use the same source of truth as the prompt index: a skill
/// in the server's project tree is not loadable from a session in another
/// project, while global config-root skills stay reachable.
#[test]
fn find_skill_scopes_to_the_session_directory() {
    let temp = tempfile::tempdir().expect("temp dir");
    let root = temp.path();
    std::fs::create_dir_all(root.join("server/.opencode")).expect("mkdir server");
    std::fs::create_dir_all(root.join("session")).expect("mkdir session");
    write_skill(root, "server/.opencode", "server-only");
    write_skill(root, "xdg/opencode", "global-one");

    let config = scoped_config(root);
    let session = root.join("session");
    let server = root.join("server");

    assert!(
        config.skill("server-only").is_some(),
        "the load-time snapshot still sees the server project"
    );
    assert!(
        find_skill(&config, &session, "server-only").is_none(),
        "a project skill outside the session tree must not be loadable"
    );
    assert_eq!(
        find_skill(&config, &session, "global-one").map(|skill| skill.id),
        Some("global-one".to_owned()),
        "global config-root skills stay visible"
    );
    assert_eq!(
        find_skill(&config, &server, "server-only").map(|skill| skill.id),
        Some("server-only".to_owned()),
        "the owning project still resolves its own skill"
    );
    assert_eq!(
        find_skill(&config, Path::new(""), "server-only").map(|skill| skill.id),
        Some("server-only".to_owned()),
        "an empty directory falls back to the load-time snapshot"
    );
    assert!(
        config
            .skills_for(&session)
            .iter()
            .all(|skill| skill.id != "server-only"),
        "the index and the loader must agree"
    );
    assert!(
        crate::prompt::skill_source(&config, "general", session.to_str().unwrap())
            .is_none_or(|source| !source.text.contains("server-only")),
        "the prompt index must not leak server skills to a session in another project"
    );
}
