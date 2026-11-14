//! End-to-end integration: load the SDK-authored example plugins into the host, verify routing,
//! and execute their hooks through a runner backed by the SDK `Plugin` trait.
//!
//! This closes the Phase 7 loop without a real WASM backend: manifests authored with the SDK load
//! and negotiate against the host, the routing table is built from their claims, and a small
//! adapter runner dispatches requests to the example plugins exactly as a transport backend would.

use std::collections::BTreeMap;

use ty_plugin_examples::{
    FieldCallReturnPlugin, ModelClassTransformPlugin, StubOverlayPlugin, call_return,
    class_transform, stub_overlay,
};
use ty_plugin_host::{LoadedPlugin, PluginEnvironment, PluginHost, PluginRunner, RuntimeError};
use ty_plugin_sdk::Plugin;
use ty_plugin_sdk::protocol::{
    AnalyzeClassRequest, CallRequest, ClassSummary, FieldSummary, PluginRequest, PluginResponse,
    SemanticContext, SymbolSource, TypeExpr,
};

/// A [`PluginRunner`] that dispatches to in-process SDK [`Plugin`] implementations, keyed by id.
///
/// A production runtime (subprocess or WASM) would serialize the request, execute the plugin
/// artifact, and deserialize the response; here we call [`Plugin::handle`] directly, which is the
/// same dispatch the SDK's wire entry point wraps.
struct SdkRunner {
    plugins: BTreeMap<String, Box<dyn Plugin>>,
}

impl SdkRunner {
    fn new(plugins: impl IntoIterator<Item = Box<dyn Plugin>>) -> Self {
        Self {
            plugins: plugins
                .into_iter()
                .map(|plugin| (plugin.manifest().id, plugin))
                .collect(),
        }
    }
}

impl PluginRunner for SdkRunner {
    fn execute(
        &self,
        plugin: &LoadedPlugin,
        request: &PluginRequest,
    ) -> Result<PluginResponse, RuntimeError> {
        self.plugins.get(plugin.id()).map_or_else(
            || {
                Err(RuntimeError::InvalidResponse(format!(
                    "no runtime registered for plugin `{}`",
                    plugin.id()
                )))
            },
            |plugin| Ok(plugin.handle(request)),
        )
    }
}

fn example_plugins() -> Vec<Box<dyn Plugin>> {
    vec![
        Box::new(StubOverlayPlugin),
        Box::new(ModelClassTransformPlugin),
        Box::new(FieldCallReturnPlugin),
    ]
}

fn host() -> PluginHost<SdkRunner> {
    let plugins = example_plugins();
    let manifests = plugins.iter().map(|plugin| plugin.manifest()).collect();
    let environment =
        PluginEnvironment::from_manifests(manifests).expect("example manifests are valid");
    PluginHost::new(environment, SdkRunner::new(plugins))
}

fn context() -> SemanticContext {
    SemanticContext {
        module: "app".to_string(),
        file_path: "/project/app.py".to_string(),
        python_version: "3.13".to_string(),
        platform: "linux".to_string(),
        config: serde_json::Value::Null,
        speculative: false,
    }
}

#[test]
fn example_manifests_negotiate_and_build_routes() {
    let host = host();
    let routes = host.environment().routes();

    assert_eq!(
        routes.class_transform_plugins(class_transform::MODEL_BASE),
        ["example.model".to_string()]
    );
    assert_eq!(
        routes.call_return_plugins(call_return::FIELD_FUNCTION),
        ["example.field".to_string()]
    );
    assert_eq!(
        routes.stub_overlay_plugins(stub_overlay::OVERLAY_MODULE),
        ["example.stub-overlay".to_string()]
    );
    assert!(routes.class_transform_plugins("app.Unclaimed").is_empty());
}

#[test]
fn host_executes_example_call_return_hook() {
    let host = host();
    let request = PluginRequest::AdjustCallReturn(CallRequest {
        context: context(),
        callee: TypeExpr::expression(call_return::FIELD_FUNCTION),
        receiver: None,
        arguments: Vec::new(),
        existing_signature: None,
        default_return_type: None,
        project_index: None,
    });

    let response = host.execute("example.field", &request).unwrap();
    let PluginResponse::CallReturnPatch(patch) = response else {
        panic!("expected a call-return patch");
    };
    assert_eq!(patch.return_type.expression, "str");
}

#[test]
fn host_executes_example_class_transform_hook() {
    let host = host();
    let request = PluginRequest::AnalyzeClass(AnalyzeClassRequest {
        context: context(),
        class: ClassSummary {
            qualified_name: "app.User".to_string(),
            bases: vec![TypeExpr::expression(class_transform::MODEL_BASE)],
            decorators: Vec::new(),
            metaclass: None,
            fields: vec![FieldSummary {
                name: "name".to_string(),
                annotation: Some(TypeExpr::annotation("str")),
                assigned_value: None,
                inferred_type: Some(TypeExpr::annotation("str")),
                has_default: false,
                source: SymbolSource::default(),
            }],
            methods: Vec::new(),
            nested_classes: Vec::new(),
            class_constants: Vec::new(),
            source: SymbolSource::default(),
        },
        project_index: None,
    });

    let response = host.execute("example.model", &request).unwrap();
    let PluginResponse::ClassPatch(patch) = response else {
        panic!("expected a class patch");
    };
    assert_eq!(patch.fields.len(), 1);
    assert_eq!(patch.constructor.expect("constructor").parameters.len(), 1);
}
