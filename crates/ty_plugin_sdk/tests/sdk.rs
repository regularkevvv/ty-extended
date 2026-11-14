//! Tests for the plugin authoring SDK: manifest building and request dispatch.

use ty_plugin_sdk::protocol::{
    CallRequest, ClassClaimKind, MethodClaimKind, MutationOperation, MutationRequest,
    MutationResponse, PluginManifest, PluginRequest, PluginResponse, ProtocolVersion, RuntimeSpec,
    SemanticContext, SymbolSource, TypeExpr,
};
use ty_plugin_sdk::{ManifestBuilder, Plugin, dsl};

#[test]
fn manifest_builder_syncs_capabilities_with_claims() {
    let manifest = ManifestBuilder::new("toy.field", "Toy field plugin", "0.1.0")
        .claim_call_return("toy.Field")
        .claim_subclass_transform("toy.Model")
        .class_batches()
        .claim_call_return_method_on_subclass("toy.Manager", "filter")
        .claim_mutations("toy.Immutable")
        .settings_module("toy.settings")
        .virtual_types()
        .stub_overlay("toy", ".ty/plugins/toy.pyi")
        .build();

    assert_eq!(manifest.id, "toy.field");
    assert_eq!(manifest.protocol_version, ProtocolVersion::CURRENT);
    assert!(matches!(manifest.runtime, RuntimeSpec::Mock));

    // Claims flip their capability flags on automatically.
    assert!(manifest.capabilities.call_return);
    assert!(manifest.capabilities.class_transform);
    assert!(manifest.capabilities.class_batch);
    assert!(manifest.capabilities.settings_data);
    assert!(manifest.capabilities.virtual_types);
    assert!(manifest.capabilities.stub_overlays);
    assert!(manifest.capabilities.mutation_validation);

    assert_eq!(manifest.claims.functions.len(), 1);
    assert_eq!(manifest.claims.functions[0].qualified_name, "toy.Field");
    assert!(manifest.claims.classes.iter().any(|claim| matches!(
        &claim.kind,
        ClassClaimKind::SubclassOf {
            base_qualified_name
        } if base_qualified_name == "toy.Model"
    )));
    assert!(manifest.claims.methods.iter().any(|claim| matches!(
        &claim.kind,
        MethodClaimKind::OnSubclassOf {
            base_qualified_name,
            method_name
        } if base_qualified_name == "toy.Manager" && method_name == "filter"
    )));
    assert!(manifest.claims.mutations.iter().any(|claim| matches!(
        &claim.kind,
        ClassClaimKind::Exact { qualified_name } if qualified_name == "toy.Immutable"
    )));
    assert_eq!(manifest.stub_overlays.len(), 1);
}

struct FieldPlugin;

impl Plugin for FieldPlugin {
    fn manifest(&self) -> PluginManifest {
        ManifestBuilder::new("toy.field", "Toy field plugin", "0.1.0")
            .claim_call_return("toy.Field")
            .build()
    }

    fn adjust_call_return(&self, _request: &CallRequest) -> PluginResponse {
        dsl::call_return(TypeExpr::annotation("str"))
    }

    fn validate_mutation(&self, _request: &MutationRequest) -> PluginResponse {
        PluginResponse::MutationDiagnostics(MutationResponse {
            diagnostics: Vec::new(),
        })
    }
}

fn call_request() -> CallRequest {
    CallRequest {
        context: SemanticContext {
            module: "app".to_string(),
            file_path: "/project/app.py".to_string(),
            python_version: "3.13".to_string(),
            platform: "linux".to_string(),
            config: serde_json::Value::Null,
            speculative: false,
        },
        callee: TypeExpr::expression("toy.Field"),
        receiver: None,
        arguments: Vec::new(),
        existing_signature: None,
        default_return_type: None,
        project_index: None,
    }
}

fn mutation_request() -> MutationRequest {
    MutationRequest {
        context: call_request().context,
        operation: MutationOperation::ItemSet,
        receiver: TypeExpr::annotation("toy.Immutable"),
        key: None,
        value: None,
        source: SymbolSource::default(),
        project_index: None,
    }
}

#[test]
fn handle_answers_manifest_request_from_manifest() {
    let plugin = FieldPlugin;
    let response = plugin.handle(&PluginRequest::Manifest);
    let PluginResponse::Manifest(manifest) = response else {
        panic!("expected a manifest response");
    };
    assert_eq!(manifest.id, "toy.field");
}

#[test]
fn handle_dispatches_to_overridden_hook() {
    let plugin = FieldPlugin;
    let response = plugin.handle(&PluginRequest::AdjustCallReturn(call_request()));
    let PluginResponse::CallReturnPatch(patch) = response else {
        panic!("expected a call-return patch");
    };
    assert_eq!(patch.return_type.expression, "str");
}

#[test]
fn handle_dispatches_mutation_validation() {
    let response = FieldPlugin.handle(&PluginRequest::ValidateMutation(Box::new(
        mutation_request(),
    )));
    assert!(matches!(response, PluginResponse::MutationDiagnostics(_)));
}

#[test]
fn handle_defaults_unimplemented_hooks_to_no_change() {
    let plugin = FieldPlugin;
    // The plugin only overrides `adjust_call_return`; the signature hook falls back to NoChange.
    let response = plugin.handle(&PluginRequest::AdjustCallSignature(call_request()));
    assert_eq!(response, PluginResponse::NoChange);
}

#[test]
fn handle_json_round_trips_through_the_wire_shape() {
    let plugin = FieldPlugin;
    let request_json = serde_json::to_string(&PluginRequest::AdjustCallReturn(call_request()))
        .expect("serialize request");

    let response_json = plugin.handle_json(&request_json).expect("dispatch");
    let response: PluginResponse = serde_json::from_str(&response_json).expect("decode response");

    let PluginResponse::CallReturnPatch(patch) = response else {
        panic!("expected a call-return patch");
    };
    assert_eq!(patch.return_type.expression, "str");
}

#[test]
fn handle_json_reports_a_decode_error_for_garbage_input() {
    let plugin = FieldPlugin;
    let error = plugin.handle_json("not json").unwrap_err();
    assert!(error.to_string().contains("decode plugin request"));
}

#[test]
fn state_claims_and_dispatch() -> Result<(), Box<dyn std::error::Error>> {
    struct StatePlugin;
    impl Plugin for StatePlugin {
        fn manifest(&self) -> PluginManifest {
            ManifestBuilder::new("state", "State", "0.1.0")
                .claim_call_state("example.Record")
                .claim_call_state_constructor("example.Exact")
                .claim_call_state_constructors_on_subclass("example.Record")
                .claim_call_state_method("example.Record", "clear")
                .claim_call_state_method_on_subclass("example.Record", "set")
                .build()
        }
        fn adjust_call_state(&self, _request: &CallRequest) -> PluginResponse {
            PluginResponse::CallStatePatch(ty_plugin_sdk::protocol::CallStatePatch {
                receiver_members: [("key".to_string(), TypeExpr::annotation("int"))].into(),
                ..Default::default()
            })
        }
    }
    let plugin = StatePlugin;
    let manifest = plugin.manifest();
    assert!(manifest.capabilities.call_state);
    assert!(!manifest.capabilities.call_return);
    assert_eq!(manifest.claims.functions.len(), 1);
    assert_eq!(manifest.claims.methods.len(), 2);
    assert_eq!(manifest.claims.constructors.len(), 2);
    let request = PluginRequest::AdjustCallState(call_request());
    let response: PluginResponse =
        serde_json::from_str(&plugin.handle_json(&serde_json::to_string(&request)?)?)?;
    let PluginResponse::CallStatePatch(patch) = response else {
        return Err("expected state patch".into());
    };
    assert_eq!(patch.receiver_members["key"].expression, "int");
    Ok(())
}
