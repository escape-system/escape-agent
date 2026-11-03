use oc_subagents::{
    BudgetError, Limits, MessageError, ModelDefaults, ModelError, ModelSource, NodeState, Policy,
    Role, SessionNode, SessionTree, SiblingBudget, SpawnBudget, SpawnError, WriteError,
    resolve_model,
};

fn node(
    id: &str,
    parent: Option<&str>,
    root: &str,
    depth: u32,
    role: Role,
    spawn_allowed: bool,
) -> SessionNode {
    SessionNode {
        id: id.to_owned(),
        parent_id: parent.map(str::to_owned),
        root_id: root.to_owned(),
        depth,
        role,
        spawn_allowed,
        child_count: 0,
        agent: "general".to_owned(),
        model: None,
        state: NodeState::Idle,
    }
}

fn root() -> SessionNode {
    node("root", None, "root", 0, Role::Primary, true)
}

fn child(id: &str, parent: &SessionNode, role: Role) -> SessionNode {
    node(
        id,
        Some(&parent.id),
        &parent.root_id,
        parent.depth + 1,
        role,
        parent.spawn_allowed,
    )
}

fn tree(nodes: &[SessionNode]) -> SessionTree {
    SessionTree::from_nodes(nodes)
}

fn agents() -> Vec<String> {
    ["general", "explore", "build"]
        .iter()
        .map(|agent| (*agent).to_owned())
        .collect()
}

#[test]
fn spawn_depth_at_limit_and_over() {
    let policy = Policy::default();
    let at_limit = node("p2", Some("root"), "root", 2, Role::Subagent, true);
    let plan = policy
        .spawn(&at_limit, "general", None, &agents())
        .expect("depth 3 is allowed with max_depth 3");
    assert_eq!(plan.depth, 3);

    let over = node("p3", Some("root"), "root", 3, Role::Subagent, true);
    assert!(matches!(
        policy.spawn(&over, "general", None, &agents()),
        Err(SpawnError::DepthExceeded { depth: 4, max: 3 })
    ));

    // A call may tighten the ceiling, never raise it.
    let shallow = node("p1", Some("root"), "root", 1, Role::Subagent, true);
    assert!(matches!(
        policy.spawn(&shallow, "general", Some(1), &agents()),
        Err(SpawnError::DepthExceeded { depth: 2, max: 1 })
    ));
    let plan = policy
        .spawn(&at_limit, "general", Some(99), &agents())
        .expect("requested depth above the config is clamped");
    assert_eq!(plan.depth, 3);
}

#[test]
fn spawn_child_cap() {
    let policy = Policy::default();
    let mut parent = root();
    parent.child_count = 8;
    assert!(matches!(
        policy.spawn(&parent, "general", None, &agents()),
        Err(SpawnError::ChildLimit {
            children: 8,
            max: 8,
            ..
        })
    ));

    parent.child_count = 7;
    assert!(policy.spawn(&parent, "general", None, &agents()).is_ok());
}

#[test]
fn spawn_allowed_flag_and_global_allow_spawn() {
    let denied = node("denied", Some("root"), "root", 1, Role::Subagent, false);
    let policy = Policy::default();
    assert!(matches!(
        policy.spawn(&denied, "general", None, &agents()),
        Err(SpawnError::SpawnNotAllowed { .. })
    ));

    let limits = Limits {
        allow_spawn: false,
        ..Limits::default()
    };
    let plan = Policy::new(limits)
        .spawn(&root(), "general", None, &agents())
        .expect("the parent may spawn; the child just cannot spawn further");
    assert!(!plan.spawn_allowed);
    assert_eq!(plan.role, Role::Subagent);
}

#[test]
fn spawn_rejects_unknown_and_primary_only_agents() {
    let policy = Policy::default();
    assert!(matches!(
        policy.spawn(&root(), "ghost", None, &agents()),
        Err(SpawnError::UnknownAgent { .. })
    ));
    assert!(matches!(
        policy.spawn(&root(), "build", None, &agents()),
        Err(SpawnError::PrimaryOnlyAgent { .. })
    ));
    assert!(policy.spawn(&root(), "explore", None, &agents()).is_ok());
}

#[test]
fn sibling_messaging_respects_the_switch() {
    let parent = root();
    let a = child("a", &parent, Role::Subagent);
    let b = child("b", &parent, Role::Subagent);
    let nodes = [parent.clone(), a.clone(), b.clone()];
    let tree = tree(&nodes);

    assert!(Policy::default().can_message(&a, &b, &tree).is_ok());

    let disabled = Policy::new(Limits {
        siblings_enabled: false,
        ..Limits::default()
    });
    assert_eq!(
        disabled.can_message(&a, &b, &tree),
        Err(MessageError::SiblingsDisabled)
    );
}

#[test]
fn child_can_message_parent_unless_disabled() {
    let parent = root();
    let a = child("a", &parent, Role::Subagent);
    let nodes = [parent.clone(), a.clone()];
    let tree = tree(&nodes);

    assert!(Policy::default().can_message(&a, &parent, &tree).is_ok());

    let no_parent = Policy::new(Limits {
        allow_parent: false,
        ..Limits::default()
    });
    assert_eq!(
        no_parent.can_message(&a, &parent, &tree),
        Err(MessageError::ParentDisabled)
    );
}

#[test]
fn parent_can_message_child() {
    let parent = root();
    let a = child("a", &parent, Role::Subagent);
    let nodes = [parent.clone(), a.clone()];
    let tree = tree(&nodes);
    assert!(Policy::default().can_message(&parent, &a, &tree).is_ok());
}

#[test]
fn coordinator_scope_covers_siblings_and_descendants() {
    let parent = root();
    let coordinator = child("coord", &parent, Role::Coordinator);
    let sibling = child("sib", &parent, Role::Subagent);
    let nephew = child("nephew", &sibling, Role::Subagent);
    let other_root = node("other", None, "other", 0, Role::Primary, true);
    let unrelated = child("unrelated", &other_root, Role::Subagent);
    let nodes = [
        parent.clone(),
        coordinator.clone(),
        sibling.clone(),
        nephew.clone(),
        other_root,
        unrelated.clone(),
    ];
    let tree = tree(&nodes);
    let policy = Policy::default();

    assert!(policy.can_message(&coordinator, &sibling, &tree).is_ok());
    assert!(policy.can_message(&coordinator, &nephew, &tree).is_ok());
    assert!(policy.can_message(&coordinator, &parent, &tree).is_ok());
    assert_eq!(
        policy.can_message(&coordinator, &unrelated, &tree),
        Err(MessageError::NotInScope {
            from: "coord".to_owned(),
            to: "unrelated".to_owned(),
        })
    );
}

#[test]
fn non_siblings_and_unknown_targets_are_denied() {
    let left = child("left", &root(), Role::Subagent);
    let right_root = node("right-root", None, "right-root", 0, Role::Primary, true);
    let right = child("right", &right_root, Role::Subagent);
    let nodes = [left.clone(), right_root, right.clone()];
    let tree = tree(&nodes);
    let policy = Policy::default();

    assert_eq!(
        policy.can_message(&left, &right, &tree),
        Err(MessageError::NotSibling {
            from: "left".to_owned(),
            to: "right".to_owned(),
        })
    );

    let ghost = node("ghost", None, "ghost", 0, Role::Subagent, false);
    assert_eq!(
        policy.can_message(&left, &ghost, &tree),
        Err(MessageError::UnknownTarget {
            id: "ghost".to_owned(),
        })
    );
}

#[test]
fn writes_are_descendant_only_except_coordinator_scope_and_self() {
    let parent = root();
    let direct = child("direct", &parent, Role::Subagent);
    let grandchild = child("grandchild", &direct, Role::Subagent);
    let sibling = child("sibling", &parent, Role::Subagent);
    let coordinator = child("coord", &parent, Role::Coordinator);
    let other_root = node("other", None, "other", 0, Role::Primary, true);
    let unrelated = child("unrelated", &other_root, Role::Subagent);
    let nodes = [
        parent.clone(),
        direct.clone(),
        grandchild.clone(),
        sibling.clone(),
        coordinator.clone(),
        other_root,
        unrelated.clone(),
    ];
    let tree = tree(&nodes);
    let policy = Policy::default();

    assert!(policy.can_write(&parent, &grandchild, &tree).is_ok());
    assert!(policy.can_write(&parent, &direct, &tree).is_ok());
    assert!(policy.can_write(&direct, &direct, &tree).is_ok());
    assert!(matches!(
        policy.can_write(&sibling, &direct, &tree),
        Err(WriteError::NotDescendant { .. })
    ));

    assert!(policy.can_write(&coordinator, &sibling, &tree).is_ok());
    assert!(policy.can_write(&coordinator, &grandchild, &tree).is_ok());
    assert_eq!(
        policy.can_write(&coordinator, &parent, &tree),
        Err(WriteError::NotInScope {
            from: "coord".to_owned(),
            to: "root".to_owned(),
        })
    );
    assert!(matches!(
        policy.can_write(&coordinator, &unrelated, &tree),
        Err(WriteError::NotInScope { .. })
    ));
}

fn available_models() -> Vec<String> {
    [
        "call/model",
        "agent/model",
        "policy/model",
        "default/model",
        "parent/model",
    ]
    .iter()
    .map(|model| (*model).to_owned())
    .collect()
}

#[test]
fn model_resolution_order() {
    let available = available_models();
    let defaults = ModelDefaults::new()
        .with_agent_model("general", "agent/model")
        .with_policy_model("general", "policy/model")
        .with_default_model("default/model");

    let choice = resolve_model(
        Some("call/model"),
        Some("general"),
        defaults.clone(),
        "parent/model",
        &available,
    )
    .expect("call wins");
    assert_eq!(choice.source, ModelSource::Call);

    let choice = resolve_model(
        None,
        Some("general"),
        defaults.clone(),
        "parent/model",
        &available,
    )
    .expect("agent config wins next");
    assert_eq!(
        choice,
        oc_subagents::ModelChoice {
            reference: "agent/model".to_owned(),
            source: ModelSource::Agent,
        }
    );

    let policy_only = ModelDefaults::new().with_policy_model("general", "policy/model");
    let choice = resolve_model(
        None,
        Some("general"),
        policy_only,
        "parent/model",
        &available,
    )
    .expect("policy model wins over the default");
    assert_eq!(choice.reference, "policy/model");

    let default_only = ModelDefaults::new().with_default_model("default/model");
    let choice = resolve_model(
        None,
        Some("general"),
        default_only,
        "parent/model",
        &available,
    )
    .expect("default wins over the parent");
    assert_eq!(choice.source, ModelSource::Default);

    let choice = resolve_model(None, None, ModelDefaults::new(), "parent/model", &available)
        .expect("parent is the last resort");
    assert_eq!(choice.source, ModelSource::Parent);
}

#[test]
fn model_errors_carry_the_candidate_chain() {
    let available = available_models();
    let defaults = ModelDefaults::new()
        .with_agent_model("general", "agent/model")
        .with_policy_model("general", "policy/model")
        .with_default_model("default/model");

    let error = resolve_model(
        Some("missing/model"),
        Some("general"),
        defaults,
        "parent/model",
        &available,
    )
    .expect_err("an unavailable call model must not fall through");
    match error {
        ModelError::Unavailable {
            reference,
            origin,
            chain,
        } => {
            assert_eq!(reference, "missing/model");
            assert_eq!(origin, "call");
            let refs: Vec<&str> = chain.iter().map(|entry| entry.reference.as_str()).collect();
            assert_eq!(
                refs,
                [
                    "missing/model",
                    "agent/model",
                    "policy/model",
                    "default/model",
                    "parent/model",
                ]
            );
        }
        other => panic!("unexpected error: {other:?}"),
    }

    let bad_variant = resolve_model(
        Some("call/model#unknown"),
        None,
        ModelDefaults::new(),
        "parent/model",
        &available,
    );
    assert!(matches!(bad_variant, Err(ModelError::Unavailable { .. })));

    let malformed = resolve_model(
        Some("nonsense"),
        None,
        ModelDefaults::new(),
        "parent/model",
        &available,
    );
    assert!(matches!(
        malformed,
        Err(ModelError::InvalidReference { .. })
    ));
}

#[test]
fn spawn_budget_allows_eight_per_minute() {
    let mut budget = SpawnBudget::new(100, 8);
    for tick in 0..8 {
        assert!(budget.try_spawn(tick * 1_000).is_ok());
    }
    assert!(matches!(
        budget.try_spawn(8_000),
        Err(BudgetError::RateLimited {
            max_per_minute: 8,
            retry_after_ms: 52_000,
        })
    ));
    assert!(budget.try_spawn(60_000).is_ok());
    assert_eq!(budget.total(), 9);

    let mut capped = SpawnBudget::new(2, 8);
    assert!(capped.try_spawn(0).is_ok());
    assert!(capped.try_spawn(0).is_ok());
    assert_eq!(
        capped.try_spawn(0),
        Err(BudgetError::SessionLimit { max: 2 })
    );
    capped.sync_active(1);
    assert!(capped.try_spawn(0).is_ok());
    assert_eq!(
        capped.try_spawn(0),
        Err(BudgetError::SessionLimit { max: 2 })
    );
}

#[test]
fn sibling_budget_allows_twenty_per_minute() {
    let mut budget = SiblingBudget::from_limits(&Limits::default());
    for tick in 0..20 {
        assert!(budget.try_message(tick * 100).is_ok());
    }
    assert!(matches!(
        budget.try_message(2_000),
        Err(BudgetError::RateLimited {
            max_per_minute: 20,
            retry_after_ms: 58_000,
        })
    ));
    assert!(budget.try_message(60_000).is_ok());
}

#[test]
fn coordinator_spawns_to_the_depth_limit() {
    let policy = Policy::default();
    let coordinator = node("coord", Some("root"), "root", 2, Role::Coordinator, true);
    let plan = policy
        .spawn(&coordinator, "general", None, &agents())
        .expect("coordinator at depth 2 may spawn at depth 3");
    assert_eq!(plan.depth, 3);
    assert_eq!(plan.role, Role::Subagent);
    assert!(plan.spawn_allowed);

    let deep = node("deep", Some("root"), "root", 3, Role::Coordinator, true);
    assert!(matches!(
        policy.spawn(&deep, "general", None, &agents()),
        Err(SpawnError::DepthExceeded { depth: 4, max: 3 })
    ));
}
