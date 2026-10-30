use std::path::Path;

use oc_config::parse_markdown;
use oc_types::EventPayload;
use serde_json::json;

use super::activate::SkillActivated;
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
