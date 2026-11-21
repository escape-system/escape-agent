//! Session-tree messaging and write authorization.
//!
//! The caller supplies the tree through [`TreeLookup`]; production code backs
//! it with `Store` session rows (parent links), tests with [`SessionTree`].

use std::collections::{BTreeMap, HashMap};

use crate::node::{Role, SessionNode};
use crate::{Limits, MessageError, WriteError};

/// Guards the ancestor walks below against corrupt trees (cycles).
const MAX_ANCESTOR_STEPS: usize = 1024;

/// Parent relationships for policy checks.
pub trait TreeLookup {
    /// Whether the session exists in the tree.
    fn contains(&self, id: &str) -> bool;

    /// The parent of `id`, or `None` for a root or an unknown id (combine with
    /// [`TreeLookup::contains`] to distinguish).
    fn parent_id(&self, id: &str) -> Option<&str>;
}

/// Simple in-memory tree, used by tests and by the server for tree endpoints.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SessionTree {
    parents: HashMap<String, Option<String>>,
}

impl SessionTree {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    pub fn insert(&mut self, node: &SessionNode) {
        self.parents.insert(node.id.clone(), node.parent_id.clone());
    }

    pub fn from_nodes<'a>(nodes: impl IntoIterator<Item = &'a SessionNode>) -> Self {
        let mut tree = Self::new();
        for node in nodes {
            tree.insert(node);
        }
        tree
    }

    #[must_use]
    pub fn len(&self) -> usize {
        self.parents.len()
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.parents.is_empty()
    }
}

impl TreeLookup for SessionTree {
    fn contains(&self, id: &str) -> bool {
        self.parents.contains_key(id)
    }

    fn parent_id(&self, id: &str) -> Option<&str> {
        self.parents.get(id).and_then(Option::as_deref)
    }
}

impl<S: std::hash::BuildHasher> TreeLookup for HashMap<String, SessionNode, S> {
    fn contains(&self, id: &str) -> bool {
        self.contains_key(id)
    }

    fn parent_id(&self, id: &str) -> Option<&str> {
        self.get(id).and_then(|node| node.parent_id.as_deref())
    }
}

impl TreeLookup for BTreeMap<String, SessionNode> {
    fn contains(&self, id: &str) -> bool {
        self.contains_key(id)
    }

    fn parent_id(&self, id: &str) -> Option<&str> {
        self.get(id).and_then(|node| node.parent_id.as_deref())
    }
}

/// True when `ancestor` appears strictly above `start` in the tree.
fn has_ancestor(tree: &dyn TreeLookup, start: &str, ancestor: &str) -> bool {
    let mut cursor = start;
    for _ in 0..MAX_ANCESTOR_STEPS {
        match tree.parent_id(cursor) {
            Some(parent) if parent == ancestor => return true,
            Some(parent) => cursor = parent,
            None => return false,
        }
    }
    false
}

/// The coordinator scope is the subtree rooted at the coordinator's parent
/// (`docs/05-subagents.md`). The parent itself is in scope for messages but is
/// excluded from writes.
fn in_coordinator_scope(
    tree: &dyn TreeLookup,
    coordinator: &str,
    target: &str,
    include_parent: bool,
) -> bool {
    let Some(root) = tree.parent_id(coordinator) else {
        return false;
    };
    if target == root {
        return include_parent;
    }
    has_ancestor(tree, target, root)
}

pub(crate) fn message_decision(
    limits: &Limits,
    from: &SessionNode,
    to: &SessionNode,
    tree: &dyn TreeLookup,
) -> Result<(), MessageError> {
    if !tree.contains(&from.id) {
        return Err(MessageError::UnknownSender {
            id: from.id.clone(),
        });
    }
    if !tree.contains(&to.id) {
        return Err(MessageError::UnknownTarget { id: to.id.clone() });
    }
    if from.id == to.id {
        return Err(MessageError::SelfMessage);
    }

    // Down to a direct child.
    if tree.parent_id(&to.id) == Some(from.id.as_str()) {
        return Ok(());
    }
    // Up to the parent.
    if tree.parent_id(&from.id) == Some(to.id.as_str()) {
        return if limits.allow_parent {
            Ok(())
        } else {
            Err(MessageError::ParentDisabled)
        };
    }
    // Coordinator scope: siblings, their descendants, and the parent (handled
    // above, where `allow_parent` still applies), symmetrically when either
    // sender or recipient is the coordinator.
    if from.role == Role::Coordinator && in_coordinator_scope(tree, &from.id, &to.id, true) {
        return Ok(());
    }
    if to.role == Role::Coordinator && in_coordinator_scope(tree, &to.id, &from.id, false) {
        return if limits.siblings_enabled {
            Ok(())
        } else {
            Err(MessageError::SiblingsDisabled)
        };
    }
    // Siblings share a non-null parent.
    let from_parent = tree.parent_id(&from.id);
    if from_parent.is_some() && from_parent == tree.parent_id(&to.id) {
        return if limits.siblings_enabled {
            Ok(())
        } else {
            Err(MessageError::SiblingsDisabled)
        };
    }
    if from.role == Role::Coordinator {
        return Err(MessageError::NotInScope {
            from: from.id.clone(),
            to: to.id.clone(),
        });
    }
    Err(MessageError::NotSibling {
        from: from.id.clone(),
        to: to.id.clone(),
    })
}

pub(crate) fn write_decision(
    from: &SessionNode,
    to: &SessionNode,
    tree: &dyn TreeLookup,
) -> Result<(), WriteError> {
    if !tree.contains(&from.id) {
        return Err(WriteError::UnknownSender {
            id: from.id.clone(),
        });
    }
    if !tree.contains(&to.id) {
        return Err(WriteError::UnknownTarget { id: to.id.clone() });
    }
    // Self-note.
    if from.id == to.id {
        return Ok(());
    }
    // Any descendant, not just a direct child.
    if has_ancestor(tree, &to.id, &from.id) {
        return Ok(());
    }
    // Coordinator may write to siblings and their descendants; never upward.
    if from.role == Role::Coordinator && in_coordinator_scope(tree, &from.id, &to.id, false) {
        return Ok(());
    }
    if from.role == Role::Coordinator {
        return Err(WriteError::NotInScope {
            from: from.id.clone(),
            to: to.id.clone(),
        });
    }
    Err(WriteError::NotDescendant {
        from: from.id.clone(),
        to: to.id.clone(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ancestor_detection_is_strict_for_self() {
        let root = SessionNode::primary("root", "build");
        let child = SessionNode::child("child", &root, Role::Subagent, "general");
        let tree = SessionTree::from_nodes([&root, &child]);
        assert!(has_ancestor(&tree, "child", "root"));
        assert!(!has_ancestor(&tree, "root", "root"));
        assert!(!has_ancestor(&tree, "root", "child"));
    }

    /// A sibling `session_write` must not just say "not a descendant" — the
    /// error has to point the agent at `talk`, the tool that actually works for
    /// sibling and parent messages. Deterministic: no store, no async, no model.
    #[test]
    fn sibling_write_error_names_talk() {
        let root = SessionNode::primary("root", "engineer");
        let a = SessionNode::child("a", &root, Role::Subagent, "general");
        let b = SessionNode::child("b", &root, Role::Subagent, "general");
        let tree = SessionTree::from_nodes([&root, &a, &b]);

        let sibling = write_decision(&a, &b, &tree).expect_err("siblings are not writable");
        let text = sibling.to_string();
        assert!(
            text.contains(
                "session_write targets descendants only; use talk to message a sibling or parent."
            ),
            "sibling write error must name `talk`: {text}"
        );

        // An upward write also fails with the same guidance: `talk` reaches a
        // parent when `limits.allow_parent` is on, `session_write` never does.
        let upward = write_decision(&a, &root, &tree).expect_err("parents are not writable");
        assert!(
            upward
                .to_string()
                .contains("use talk to message a sibling or parent."),
            "upward write error must name `talk`: {upward}"
        );
    }

    #[test]
    fn worker_in_subtree_can_message_coordinator_symmetrically() {
        let root = SessionNode::primary("root", "engineer");
        let coord = SessionNode::child("coord", &root, Role::Coordinator, "general");
        let other = SessionNode::child("other", &root, Role::Subagent, "general");
        let worker = SessionNode::child("worker", &other, Role::Subagent, "general");
        let tree = SessionTree::from_nodes([&root, &coord, &other, &worker]);

        let mut limits = Limits {
            siblings_enabled: true,
            ..Limits::default()
        };
        assert!(message_decision(&limits, &coord, &worker, &tree).is_ok());
        assert!(message_decision(&limits, &worker, &coord, &tree).is_ok());

        limits.siblings_enabled = false;
        assert_eq!(
            message_decision(&limits, &worker, &coord, &tree),
            Err(MessageError::SiblingsDisabled)
        );
    }
}
