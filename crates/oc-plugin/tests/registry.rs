//! Native registry: ordering, duplicates, guards, transforms, hooks, RPC.

use std::sync::{Arc, Mutex};

use oc_plugin::{
    Domain, Hook, Plugin, PluginError, PluginInfo, PluginRegistry, PluginSource, PluginState,
    PluginUpdatedEvent, RpcError, RpcNamespace, Tier, TransformDoc, is_guarded_id,
};
use serde_json::{Value, json};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Behavior {
    Record,
    Mutate,
    FailToolBefore,
    FailAfter,
    Panic,
}

struct TestPlugin {
    id: String,
    label: &'static str,
    behavior: Behavior,
    calls: Arc<Mutex<Vec<String>>>,
}

impl TestPlugin {
    fn new(
        id: &str,
        label: &'static str,
        behavior: Behavior,
        calls: &Arc<Mutex<Vec<String>>>,
    ) -> Self {
        Self {
            id: id.to_string(),
            label,
            behavior,
            calls: Arc::clone(calls),
        }
    }
}

impl Plugin for TestPlugin {
    fn id(&self) -> &str {
        &self.id
    }

    fn transform(&self, _domain: Domain, doc: &mut TransformDoc) {
        self.calls.lock().unwrap().push(self.label.to_string());
        match self.behavior {
            Behavior::Mutate => push_order(doc, self.label),
            Behavior::Panic => panic!("transform exploded"),
            Behavior::Record | Behavior::FailToolBefore | Behavior::FailAfter => {}
        }
    }

    fn hook(&self, hook: Hook, event: &mut Value) -> Result<(), PluginError> {
        self.calls.lock().unwrap().push(self.label.to_string());
        match self.behavior {
            Behavior::Panic => panic!("hook exploded"),
            Behavior::FailToolBefore if hook == Hook::ToolExecuteBefore => {
                Err(PluginError::HookFailed {
                    plugin: self.id.clone(),
                    hook,
                    message: "denied".to_string(),
                    metadata: Some(json!({"reason": "test"})),
                })
            }
            Behavior::FailAfter if hook == Hook::ToolExecuteAfter => Err(PluginError::HookFailed {
                plugin: self.id.clone(),
                hook,
                message: "after failed".to_string(),
                metadata: None,
            }),
            Behavior::Mutate
            | Behavior::FailToolBefore
            | Behavior::FailAfter
            | Behavior::Record => {
                push_order(event, self.label);
                Ok(())
            }
        }
    }
}

fn push_order(value: &mut Value, label: &str) {
    let Value::Object(object) = value else {
        return;
    };
    let entry = object
        .entry("order".to_string())
        .or_insert_with(|| Value::Array(Vec::new()));
    if let Value::Array(items) = entry {
        items.push(Value::from(label));
    }
}

fn calls() -> Arc<Mutex<Vec<String>>> {
    Arc::new(Mutex::new(Vec::new()))
}

fn labels(calls: &Arc<Mutex<Vec<String>>>) -> Vec<String> {
    calls.lock().unwrap().clone()
}

struct EchoPlugin {
    id: String,
    namespace: String,
}

impl Plugin for EchoPlugin {
    fn id(&self) -> &str {
        &self.id
    }

    fn rpc(&self) -> Option<&dyn RpcNamespace> {
        Some(self)
    }
}

impl RpcNamespace for EchoPlugin {
    fn namespace(&self) -> &str {
        &self.namespace
    }

    fn invoke(&self, method: &str, input: Value) -> Result<Value, RpcError> {
        if method != "echo" {
            return Err(RpcError::MethodNotFound {
                namespace: self.namespace.clone(),
                method: method.to_string(),
            });
        }
        Ok(input)
    }
}

#[test]
fn dispatch_follows_activation_order() {
    let calls = calls();
    let registry = PluginRegistry::new();
    registry.register(
        Tier::Package,
        TestPlugin::new("package", "package", Behavior::Record, &calls),
    );
    registry.register(
        Tier::BuiltinPost,
        TestPlugin::new("post", "post", Behavior::Record, &calls),
    );
    registry.register(
        Tier::BuiltinPre,
        TestPlugin::new("pre", "pre", Behavior::Record, &calls),
    );
    registry.register(
        Tier::Instance,
        TestPlugin::new("instance", "instance", Behavior::Record, &calls),
    );
    registry.register(
        Tier::Sdk,
        TestPlugin::new("sdk", "sdk", Behavior::Record, &calls),
    );

    let mut doc = json!({});
    registry.transform(Domain::Agent, &mut doc);

    assert_eq!(
        labels(&calls),
        vec![
            "pre".to_string(),
            "sdk".to_string(),
            "instance".to_string(),
            "package".to_string(),
            "post".to_string()
        ]
    );
}

#[test]
fn transform_mutations_are_sequential_and_visible() {
    let calls = calls();
    let registry = PluginRegistry::new();
    registry.register(
        Tier::Instance,
        TestPlugin::new("one", "one", Behavior::Mutate, &calls),
    );
    registry.register(
        Tier::Package,
        TestPlugin::new("two", "two", Behavior::Mutate, &calls),
    );

    let mut doc = json!({"agents": []});
    registry.transform(Domain::Agent, &mut doc);

    assert_eq!(doc["order"], json!(["one", "two"]));
    assert_eq!(doc["agents"], json!([]));
}

#[test]
fn hook_mutations_are_sequential() {
    let calls = calls();
    let registry = PluginRegistry::new();
    registry.register(
        Tier::BuiltinPre,
        TestPlugin::new("one", "one", Behavior::Mutate, &calls),
    );
    registry.register(
        Tier::Package,
        TestPlugin::new("two", "two", Behavior::Mutate, &calls),
    );

    let mut event = json!({"sessionID": "ses_1"});
    registry.hook(Hook::SessionPrompt, &mut event).unwrap();

    assert_eq!(event["order"], json!(["one", "two"]));
    assert_eq!(event["sessionID"], "ses_1");
}

#[test]
fn tool_before_failure_aborts_dispatch() {
    let calls = calls();
    let registry = PluginRegistry::new();
    registry.register(
        Tier::BuiltinPre,
        TestPlugin::new("deny", "deny", Behavior::FailToolBefore, &calls),
    );
    registry.register(
        Tier::Package,
        TestPlugin::new("later", "later", Behavior::Mutate, &calls),
    );

    let mut event = json!({"tool": "shell"});
    let error = registry
        .hook(Hook::ToolExecuteBefore, &mut event)
        .unwrap_err();
    match error {
        PluginError::HookFailed {
            plugin,
            hook,
            message,
            metadata,
        } => {
            assert_eq!(plugin, "deny");
            assert_eq!(hook, Hook::ToolExecuteBefore);
            assert_eq!(message, "denied");
            assert_eq!(metadata, Some(json!({"reason": "test"})));
        }
        other => panic!("unexpected error: {other:?}"),
    }
    assert_eq!(labels(&calls), vec!["deny".to_string()]);
    assert_eq!(event, json!({"tool": "shell"}));
}

#[test]
fn infallible_hook_error_is_ignored() {
    let calls = calls();
    let registry = PluginRegistry::new();
    registry.register(
        Tier::BuiltinPre,
        TestPlugin::new("fail", "fail", Behavior::FailAfter, &calls),
    );
    registry.register(
        Tier::Package,
        TestPlugin::new("later", "later", Behavior::Mutate, &calls),
    );

    let mut event = json!({"tool": "read"});
    registry.hook(Hook::ToolExecuteAfter, &mut event).unwrap();
    assert_eq!(
        labels(&calls),
        vec!["fail".to_string(), "later".to_string()]
    );
    assert_eq!(event["order"], json!(["later"]));
}

#[test]
fn duplicate_id_keeps_first_and_marks_rest_failed() {
    let calls = calls();
    let registry = PluginRegistry::new();
    let first = registry.register(
        Tier::Package,
        TestPlugin::new("dup", "first", Behavior::Record, &calls),
    );
    let second = registry.register(
        Tier::BuiltinPre,
        TestPlugin::new("dup", "second", Behavior::Record, &calls),
    );

    assert!(first.state.is_active());
    assert_eq!(second.state, PluginState::failed("duplicate id"));

    let inventory = registry.inventory();
    assert_eq!(inventory.len(), 2);
    let active = inventory
        .iter()
        .filter(|info| info.id == "dup" && info.is_active())
        .count();
    let failed = inventory
        .iter()
        .filter(|info| info.state == PluginState::failed("duplicate id"))
        .count();
    assert_eq!(active, 1);
    assert_eq!(failed, 1);

    let mut doc = json!({});
    registry.transform(Domain::Skill, &mut doc);
    assert_eq!(labels(&calls), vec!["first".to_string()]);
}

#[test]
fn guarded_ids_cannot_be_removed() {
    let calls = calls();
    let registry = PluginRegistry::new();
    assert!(is_guarded_id("opencode.provider.anthropic"));
    registry.register(
        Tier::BuiltinPre,
        TestPlugin::new(
            "opencode.provider.mock",
            "provider",
            Behavior::Record,
            &calls,
        ),
    );
    registry.register(
        Tier::Package,
        TestPlugin::new("local.plugin", "local", Behavior::Record, &calls),
    );
    registry.guard("local.plugin");

    for id in ["opencode.provider.mock", "local.plugin"] {
        assert!(registry.is_guarded(id));
        let error = registry.remove(id).unwrap_err();
        assert_eq!(error, PluginError::Guarded { id: id.to_string() });
    }
    assert_eq!(registry.len(), 2);

    registry.unguard("local.plugin");
    assert!(registry.remove("local.plugin").unwrap());
    assert_eq!(registry.len(), 1);
}

#[test]
fn panic_disables_plugin_and_dispatch_continues() {
    let calls = calls();
    let registry = PluginRegistry::new();
    registry.register(
        Tier::BuiltinPre,
        TestPlugin::new("boom", "boom", Behavior::Panic, &calls),
    );
    registry.register(
        Tier::Package,
        TestPlugin::new("later", "later", Behavior::Record, &calls),
    );

    let mut doc = json!({});
    registry.transform(Domain::Agent, &mut doc);
    assert_eq!(
        labels(&calls),
        vec!["boom".to_string(), "later".to_string()]
    );

    let info = registry.info("boom").unwrap();
    assert!(info.state.error().unwrap().contains("panic"));

    let mut event = json!({});
    registry.hook(Hook::SessionPrompt, &mut event).unwrap();
    assert_eq!(event["order"], json!(["later"]));
}

#[test]
fn rpc_namespace_dispatch_and_collision() {
    let registry = PluginRegistry::new();
    let first = registry.register(
        Tier::Package,
        EchoPlugin {
            id: "echo.one".to_string(),
            namespace: "test".to_string(),
        },
    );
    let second = registry.register(
        Tier::BuiltinPost,
        EchoPlugin {
            id: "echo.two".to_string(),
            namespace: "test".to_string(),
        },
    );
    assert!(first.state.is_active());
    assert_eq!(
        second.state,
        PluginState::failed("duplicate rpc namespace: test")
    );
    assert_eq!(registry.namespaces(), vec!["test".to_string()]);
    assert!(registry.has_namespace("test"));

    let output = registry
        .invoke("test", "echo", json!({"hello": "world"}))
        .unwrap();
    assert_eq!(output, json!({"hello": "world"}));

    let missing = registry.invoke("absent", "echo", Value::Null).unwrap_err();
    assert_eq!(missing.code(), -32601);
    assert!(matches!(missing, RpcError::NamespaceNotFound { .. }));

    let method = registry.invoke("test", "missing", Value::Null).unwrap_err();
    assert!(matches!(method, RpcError::MethodNotFound { .. }));
    assert_eq!(method.code(), -32601);

    let wire = method.to_object();
    assert_eq!(wire.code, -32601);
    assert!(wire.data.is_some());
}

#[test]
fn inventory_wire_shape_matches_upstream_schema() {
    let registry = PluginRegistry::new();
    registry.register(
        Tier::BuiltinPre,
        TestPlugin::new("builtin.id", "x", Behavior::Record, &calls()),
    );
    let info: PluginInfo = registry.info("builtin.id").unwrap();
    assert_eq!(
        serde_json::to_value(&info).unwrap(),
        json!({
            "id": "builtin.id",
            "source": {"type": "builtin"},
            "features": {"server": true},
            "state": {"status": "active"}
        })
    );

    let failed = PluginInfo {
        id: "failed.id".to_string(),
        source: PluginSource::Package {
            target: "pkg".to_string(),
            version: Some("1.2.3".to_string()),
            outdated: None,
            updating: Some(true),
        },
        features: oc_plugin::PluginFeatures::server_with_rpc(),
        state: PluginState::failed("duplicate id"),
    };
    assert_eq!(
        serde_json::to_value(&failed).unwrap(),
        json!({
            "id": "failed.id",
            "source": {"type": "package", "target": "pkg", "version": "1.2.3", "updating": true},
            "features": {"server": true, "rpc": true},
            "state": {"status": "failed", "error": "duplicate id"}
        })
    );

    // Round-trip.
    let decoded: PluginInfo =
        serde_json::from_value(serde_json::to_value(&failed).unwrap()).unwrap();
    assert_eq!(decoded, failed);

    assert_eq!(
        PluginUpdatedEvent::new().to_value(),
        json!({"type": "plugin.updated"})
    );
    assert_eq!(oc_plugin::PLUGIN_UPDATED, "plugin.updated");
}
