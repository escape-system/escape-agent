//! Wildcard coverage of saved rules.

use crate::permission::state::{Rule, covered};

#[test]
fn covered_uses_wildcard_rules() {
    let rules = vec![Rule {
        action: "read".to_owned(),
        resources: vec!["*.env".to_owned()],
        durable: true,
    }];
    assert!(covered(&rules, "read", &["config/.env".to_owned()]));
    assert!(!covered(&rules, "read", &["config/.env.local".to_owned()]));
    assert!(!covered(&rules, "write", &["config/.env".to_owned()]));
    let star = vec![Rule {
        action: "*".to_owned(),
        resources: vec!["*".to_owned()],
        durable: true,
    }];
    assert!(covered(&star, "shell", &["rm x".to_owned()]));
}
