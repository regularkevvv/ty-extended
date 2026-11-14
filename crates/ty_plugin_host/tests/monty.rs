//! End-to-end tests for the Monty plugin backend.
//!
//! These run only when `plugins-monty` is enabled and drive real Python plugin source through the
//! in-process interpreter: the SDK prelude is prepended by the runner, the plugin registers hooks
//! with it, and requests cross the boundary as serialized JSON. Failure-path tests override
//! `__ty_handle__` or use malformed source so each error mapping is exercised deterministically.

#![cfg(all(feature = "plugins-monty", not(target_arch = "wasm32")))]

use ty_plugin_host::{MontyLimits, MontyRunner, PluginEnvironment, PluginHost};
use ty_plugin_protocol::{
    ArgumentKind, ArgumentSummary, CallRequest, DependencyRequest, LiteralValue, PluginDependency,
    PluginRequest, PluginResponse, RuntimeSpec, SemanticContext, TypeExpr,
};
use ty_plugin_sdk::ManifestBuilder;

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

fn call_request() -> PluginRequest {
    PluginRequest::AdjustCallReturn(CallRequest {
        context: context(),
        callee: TypeExpr::expression("example.runner"),
        receiver: None,
        arguments: vec![ArgumentSummary {
            name: None,
            kind: ArgumentKind::Positional,
            type_expr: Some(TypeExpr::annotation("str")),
            value: LiteralValue::Str {
                value: "token".to_string(),
            },
            source: None,
        }],
        existing_signature: None,
        default_return_type: None,
        project_index: None,
    })
}

/// A plugin that claims `example.runner` calls and rewrites their return type to `Token`.
const EXAMPLE_PLUGIN: &str = r#"
set_manifest(manifest(
    id="example.runner",
    name="Example Runner",
    version="0.1.0",
    capabilities=capabilities(call_return=True),
    claims={"functions": [{"qualified-name": "example.runner"}]},
))

@on_call_return
def adjust_runner(request):
    return call_return_patch(type_expr("example.Token", mode="annotation"))
"#;

fn example_host(limits: MontyLimits) -> PluginHost<MontyRunner> {
    source_host(limits, EXAMPLE_PLUGIN)
}

fn source_host(limits: MontyLimits, source: &str) -> PluginHost<MontyRunner> {
    source_host_with_lookup(limits, source, false)
}

fn source_host_with_lookup(
    limits: MontyLimits,
    source: &str,
    lookup: bool,
) -> PluginHost<MontyRunner> {
    let mut manifest = ManifestBuilder::new("example.runner", "Example Runner", "0.1.0")
        .runtime(RuntimeSpec::Monty(
            ty_plugin_sdk::protocol::MontyRuntimeSpec {
                artifact: "plugin.py".to_string(),
                sha256: None,
            },
        ))
        .build();
    manifest.capabilities.project_index_lookup = lookup;
    let plugin_id = manifest.id.clone();
    let environment =
        PluginEnvironment::from_manifests(vec![manifest]).expect("example manifest is valid");
    let mut runner = MontyRunner::new(limits).expect("runner builds");
    runner
        .add_plugin(plugin_id, source)
        .expect("plugin source compiles");
    PluginHost::new(environment, runner)
}

#[test]
fn project_index_hooks_can_read_bounded_fresh_snapshot_lookups() {
    let source = r#"
@on_project_index
def build(request):
    result = project_index_value(request, ['source'], {})
    assert result == {'name': 'café😀', 'fields': ['one']}
    result['fields'].append('mutated')
    assert project_index_value(request, ['source'])['fields'] == ['one']
    return project_index({'source': project_index_value(request, ['source'])})
"#;
    let host = source_host_with_lookup(MontyLimits::default(), source, true);
    let request = PluginRequest::BuildProjectIndex(ty_plugin_protocol::BuildProjectIndexRequest {
        context: ty_plugin_protocol::ProjectContext {
            root: "/project".into(),
            python_version: "3.13".into(),
            platform: "linux".into(),
            config: serde_json::json!({}),
        },
        classes: Vec::new(),
        settings: Vec::new(),
        assignments: Vec::new(),
        functions: Vec::new(),
        previous_index_fingerprint: None,
    });
    let index = serde_json::json!({"source":{"name":"café😀","fields":["one"]}});
    let before = index.clone();
    let response = host
        .execute_with_project_index("example.runner", &request, &index)
        .expect("project hook lookup");
    assert_eq!(
        serde_json::to_value(response).unwrap()["plugin-index"]["source"],
        index["source"]
    );
    assert_eq!(index, before);
    let limited = source_host_with_lookup(
        MontyLimits {
            max_response_bytes: 16,
            ..MontyLimits::default()
        },
        source,
        true,
    );
    assert!(
        limited
            .execute_with_project_index("example.runner", &request, &index)
            .is_err()
    );
}

#[test]
fn parallel_indexed_calls_preserve_isolation_and_serial_results() {
    let host = source_host_with_lookup(
        MontyLimits::default(),
        r#"
counter = 0
@on_call_return
def answer(request):
    global counter
    counter += 1
    assert counter == 1
    value = project_index_value(request, ["annotation"])
    return call_return_patch(type_annotation(value))
"#,
        true,
    );
    let annotations = ["str", "int", "bytes", "bool"];
    let expected = annotations.map(|annotation| {
        let index = serde_json::json!({"annotation": annotation});
        host.execute_with_project_index("example.runner", &call_request(), &index)
            .expect("serial indexed call succeeds")
    });
    std::thread::scope(|scope| {
        let handles: Vec<_> = annotations
            .iter()
            .enumerate()
            .map(|(position, annotation)| {
                let host = &host;
                let expected = &expected[position];
                scope.spawn(move || {
                    let index = serde_json::json!({"annotation": annotation});
                    for _ in 0..16 {
                        let actual = host
                            .execute_with_project_index("example.runner", &call_request(), &index)
                            .expect("parallel indexed call succeeds");
                        assert_eq!(&actual, expected);
                        assert_eq!(index["annotation"], *annotation);
                    }
                })
            })
            .collect();
        for handle in handles {
            handle.join().expect("indexed worker completes");
        }
    });
}

#[test]
fn stateless_batches_match_serial_and_keep_requests_and_indexes_independent() {
    let source = r#"
@on_call_return
def answer(request):
    assert request['arguments'][0]['value']['value'] == 'token'
    request['arguments'][0]['value']['value'] = 'local mutation'
    record = project_index_value(request, ['record'])
    annotation = record['annotation']
    record['annotation'] = 'local mutation'
    assert project_index_value(request, ['record', 'annotation']) == annotation
    return call_return_patch(type_annotation(annotation))
"#;
    for lookup in [false, true] {
        let host = source_host_with_lookup(MontyLimits::default(), source, lookup);
        let request = call_request();
        let requests = vec![&request; 16];
        std::thread::scope(|scope| {
            let workers: Vec<_> = ["str", "int", "bytes", "bool"]
                .into_iter()
                .map(|annotation| {
                    let host = &host;
                    let requests = &requests;
                    let request = &request;
                    scope.spawn(move || {
                        let index = serde_json::json!({"record":{"annotation":annotation}});
                        let expected = host
                            .execute_with_project_index("example.runner", request, &index)
                            .expect("ordinary call");
                        for _ in 0..4 {
                            let actual = host
                                .execute_stateless_batch_with_project_index(
                                    "example.runner",
                                    requests,
                                    &index,
                                )
                                .expect("stateless batch");
                            assert_eq!(actual.len(), requests.len());
                            assert!(actual.iter().all(|response| response == &expected));
                            assert_eq!(index["record"]["annotation"], annotation);
                        }
                    })
                })
                .collect();
            for worker in workers {
                worker.join().expect("independent indexed batch");
            }
        });
        assert!(
            host.execute_stateless_batch_with_project_index(
                "example.runner",
                &[&request; 65],
                &serde_json::json!({})
            )
            .is_err()
        );
        assert!(
            host.execute_stateless_batch_with_project_index(
                "example.runner",
                &[],
                &serde_json::json!({})
            )
            .expect("empty batch")
            .is_empty()
        );
    }
}

#[test]
fn stateless_batch_failures_keep_response_limits_and_ordinary_calls() {
    let host = source_host_with_lookup(
        MontyLimits {
            max_response_bytes: 512,
            ..MontyLimits::default()
        },
        r#"
@on_call_return
def answer(request):
    return call_return_patch(type_annotation('str'))
"#,
        true,
    );
    let request = call_request();
    let index = serde_json::json!({});
    assert!(
        host.execute_with_project_index("example.runner", &request, &index)
            .is_ok()
    );
    assert!(
        host.execute_stateless_batch_with_project_index("example.runner", &[&request; 16], &index)
            .is_err()
    );
    assert!(
        host.execute_with_project_index("example.runner", &request, &index)
            .is_ok()
    );
    for result in ["[]", "[42]", "[\"not JSON\",\"{}\"]"] {
        let source = format!(
            "def __ty_handle_many__(payload):\n    return {result:?}\n@on_call_return\ndef answer(request):\n    return call_return_patch(type_annotation('str'))\n"
        );
        let host = source_host_with_lookup(MontyLimits::default(), &source, true);
        assert!(
            host.execute_stateless_batch_with_project_index(
                "example.runner",
                &[&request, &request],
                &index
            )
            .is_err()
        );
        assert!(
            host.execute_with_project_index("example.runner", &request, &index)
                .is_ok()
        );
    }
}

#[test]
fn index_lookup_inline_fallback_and_borrowed_values_agree() {
    let source = r#"
@on_call_return
def answer(request):
    assert project_index_keys(request, ["models"]) == ["a/b", "qualified.name"]
    assert project_index_value(request, ["models", "missing"], "default") == "default"
    assert project_index_value(request, ["models", "a/b"], "default") is None
    record = project_index_value(request, ["models", "qualified.name"])
    record["annotation"] = "bytes"
    return call_return_patch(type_annotation(project_index_value(request, ["models", "qualified.name", "annotation"])))
"#;
    let index = serde_json::json!({"models":{"a/b":null,"qualified.name":{"annotation":"int"}}});
    for lookup in [false, true] {
        let host = source_host_with_lookup(MontyLimits::default(), source, lookup);
        let response = host
            .execute_with_project_index("example.runner", &call_request(), &index)
            .expect("selected index is available");
        let PluginResponse::CallReturnPatch(patch) = response else {
            panic!("expected return patch, got {response:?}");
        };
        assert_eq!(patch.return_type.expression, "int");
        assert_eq!(index["models"]["qualified.name"]["annotation"], "int");
    }
}

#[test]
fn index_lookup_rejects_invalid_requests_and_bounds_replies_and_suspensions() {
    let calls = [
        (
            "_ty_project_index_lookup('value')",
            "invalid project index lookup",
        ),
        (
            "_ty_project_index_lookup('value', '[]', extra=1)",
            "invalid project index lookup",
        ),
        (
            "_ty_project_index_lookup('value', 17)",
            "invalid project index lookup",
        ),
        (
            "_ty_project_index_lookup('value', 'not json')",
            "invalid project index lookup",
        ),
        (
            "_ty_project_index_lookup('value', '[0]')",
            "invalid project index lookup",
        ),
        (
            "_ty_project_index_lookup('delete', '[]')",
            "invalid project index lookup",
        ),
        (
            "_ty_project_index_lookup('value', json.dumps(['x'] * 129))",
            "invalid project index lookup",
        ),
        (
            "_ty_project_index_lookup('value', ' ' * 65537)",
            "invalid project index lookup",
        ),
        (
            "_ty_project_index_lookup('value', '[]')",
            "exceeds the response limit",
        ),
    ];
    let index = serde_json::json!({"unused":"x".repeat(1024)});
    for (call, expected) in calls {
        let source = format!(
            "def __ty_handle__(request):\n    {call}\n    return '{{\"kind\":\"no-change\"}}'\n"
        );
        let limits = MontyLimits {
            max_response_bytes: 512,
            ..MontyLimits::default()
        };
        let host = source_host_with_lookup(limits, &source, true);
        let error = host
            .execute_with_project_index("example.runner", &call_request(), &index)
            .expect_err("invalid lookup is rejected");
        assert!(error.to_string().contains(expected), "{call}: {error}");
    }
    let host = source_host_with_lookup(
        MontyLimits::default(),
        r#"
def __ty_handle__(request):
    for _ in range(1001):
        _ty_project_index_lookup("keys", "[]")
    return '{"kind":"no-change"}'
"#,
        true,
    );
    let error = host
        .execute_with_project_index("example.runner", &call_request(), &serde_json::json!({}))
        .expect_err("host bounds suspension loops");
    assert!(
        error.to_string().contains("lookup limit exceeded"),
        "{error}"
    );
}

#[test]
fn index_lookup_does_not_grant_filesystem_access() {
    let directory = tempfile::tempdir().expect("temporary directory");
    let path = directory.path().join("host-only.txt");
    std::fs::write(&path, "host-only contents").expect("write canary");
    let source = format!(
        "from pathlib import Path\ndef __ty_handle__(request):\n    Path({}).read_text()\n    return '{{\"kind\":\"no-change\"}}'\n",
        serde_json::to_string(&path.to_string_lossy()).unwrap(),
    );
    let host = source_host_with_lookup(MontyLimits::default(), &source, true);
    let error = host
        .execute_with_project_index("example.runner", &call_request(), &serde_json::json!({}))
        .expect_err("index capability cannot service filesystem operations");
    assert!(
        error.to_string().contains("no other external operations"),
        "{error}"
    );
    assert!(!error.to_string().contains("host-only contents"));
    assert_eq!(std::fs::read_to_string(path).unwrap(), "host-only contents");
}

#[test]
fn runs_python_plugin_in_monty() {
    let host = example_host(MontyLimits::default());

    let manifest_response = host
        .execute("example.runner", &PluginRequest::Manifest)
        .expect("manifest request executes");
    let PluginResponse::Manifest(manifest) = manifest_response else {
        panic!("expected a manifest, got {manifest_response:?}");
    };
    assert_eq!(manifest.id, "example.runner");
    assert!(manifest.capabilities.call_return);

    let response = host
        .execute("example.runner", &call_request())
        .expect("call-return request executes");
    let PluginResponse::CallReturnPatch(patch) = response else {
        panic!("expected a call-return patch, got {response:?}");
    };
    assert_eq!(patch.return_type.expression, "example.Token");
}

#[test]
fn unclaimed_hooks_get_no_change() {
    let host = example_host(MontyLimits::default());

    let request = PluginRequest::AdjustCallSignature(CallRequest {
        context: context(),
        callee: TypeExpr::expression("example.runner"),
        receiver: None,
        arguments: Vec::new(),
        existing_signature: None,
        default_return_type: None,
        project_index: None,
    });
    let response = host
        .execute("example.runner", &request)
        .expect("request executes");

    assert_eq!(response, PluginResponse::NoChange);
}

#[test]
fn sdk_builders_produce_valid_wire_responses() {
    const BUILDER_PLUGIN: &str = r#"
set_manifest(manifest(
    id="example.builders",
    name="Example Builders",
    version="0.1.0",
    capabilities=capabilities(
        call_signature=True,
        call_return=True,
        project_index=True,
        additional_dependencies=True,
    ),
))

@on_call_signature
def signature(request):
    return call_signature_patch(
        callable_signature(
            parameters=[
                parameter(
                    name="value",
                    type=type_expr("str", mode="annotation"),
                    required=True,
                ),
            ],
            return_type=type_expr("example.Token", mode="annotation"),
        )
    )

@on_dependencies
def deps(request):
    return dependencies([dependency("vendor/spec.json")])

@on_project_index
def index(request):
    return project_index({"tokens": {"example.issue_token": "example.Token"}})
"#;
    let manifest = ManifestBuilder::new("example.builders", "test", "0.0.0").build();
    let environment = PluginEnvironment::from_manifests(vec![manifest]).expect("manifest is valid");
    let plugin_id = "example.builders".to_string();
    let mut runner = MontyRunner::new(MontyLimits::default()).expect("runner builds");
    runner
        .add_plugin(&plugin_id, BUILDER_PLUGIN)
        .expect("plugin source compiles");
    let host = PluginHost::new(environment, runner);

    let response = host
        .execute(
            &plugin_id,
            &PluginRequest::AdjustCallSignature(CallRequest {
                context: context(),
                callee: TypeExpr::expression("example.issue_token"),
                receiver: None,
                arguments: Vec::new(),
                existing_signature: None,
                default_return_type: None,
                project_index: None,
            }),
        )
        .expect("call-signature request executes");
    let PluginResponse::CallSignaturePatch(patch) = response else {
        panic!("expected a call-signature patch, got {response:?}");
    };
    assert_eq!(patch.signature.return_type.expression, "example.Token");
    assert_eq!(patch.signature.parameters[0].name.as_deref(), Some("value"));

    let response = host
        .execute(
            &plugin_id,
            &PluginRequest::AdditionalDependencies(DependencyRequest {
                config: serde_json::Value::default(),
            }),
        )
        .expect("dependencies request executes");
    let PluginResponse::Dependencies(deps) = response else {
        panic!("expected a dependencies response, got {response:?}");
    };
    assert_eq!(
        deps.dependencies,
        vec![PluginDependency {
            path: "vendor/spec.json".to_string(),
            sha256: None,
        }]
    );

    let response = host
        .execute(
            &plugin_id,
            &PluginRequest::BuildProjectIndex(ty_plugin_protocol::BuildProjectIndexRequest {
                context: ty_plugin_protocol::ProjectContext {
                    root: "/project".to_string(),
                    python_version: "3.13".to_string(),
                    platform: "linux".to_string(),
                    config: serde_json::Value::default(),
                },
                classes: Vec::new(),
                settings: Vec::new(),
                assignments: Vec::new(),
                functions: Vec::new(),
                previous_index_fingerprint: None,
            }),
        )
        .expect("project-index request executes");
    let PluginResponse::ProjectIndex(index) = response else {
        panic!("expected a project-index response, got {response:?}");
    };
    assert_eq!(
        index.plugin_index["tokens"]["example.issue_token"],
        "example.Token"
    );
}

#[test]
fn sdk_surface_covers_manifest_claims_and_project_index() {
    const SURFACE_PLUGIN: &str = r#"
set_manifest(manifest(
    id="example.surface",
    name="Example Surface",
    version="0.1.0",
    capabilities=capabilities(project_index=True, call_return=True),
    claims=claims(
        functions=[symbol_claim("example.issue_token")],
        methods=[method_claim_on_subclass_of_matching("example.Base", "run_*")],
        attributes=[attribute_claim_exact("example.Widget", "state", "instance")],
        mutations=[class_claim_subclass_of("example.Model")],
        settings=[settings_claim(module="example.settings")],
    ),
    stub_overlays=[stub_overlay("example", "stubs/example.pyi")],
))

@on_call_return_of("example.runner")
def filtered(request):
    args = call_arguments(request)
    if literal_value(args[0]) == "token":
        return call_return_patch(type_expr("example.Token", mode="annotation"))
    return None

@on_call_return
def fallback(request):
    return call_return_patch(type_expr("example.Fallback", mode="annotation"))

@on_project_index
def index(request):
    return project_index(
        {"seen": True},
        contributions=[
            contribution(
                symbol_source(module="example", qualified_name="example.tokens"),
                instance_target("example.Widget"),
                member_contribution(member("state", type_annotation("int"))),
                "example.tokens/state",
            ),
        ],
        virtual_types=[
            virtual_type(
                "example.Pair",
                virtual_named_tuple([
                    virtual_field("first", type_annotation("int")),
                    virtual_field("second", type_annotation("str"), required=False),
                ]),
            ),
        ],
    )
"#;
    let manifest = ManifestBuilder::new("example.surface", "test", "0.0.0").build();
    let environment = PluginEnvironment::from_manifests(vec![manifest]).expect("manifest is valid");
    let plugin_id = "example.surface".to_string();
    let mut runner = MontyRunner::new(MontyLimits::default()).expect("runner builds");
    runner
        .add_plugin(&plugin_id, SURFACE_PLUGIN)
        .expect("plugin source compiles");
    let host = PluginHost::new(environment, runner);

    let response = host
        .execute(&plugin_id, &PluginRequest::Manifest)
        .expect("manifest request executes");
    let PluginResponse::Manifest(manifest) = response else {
        panic!("expected a manifest, got {response:?}");
    };
    assert_eq!(manifest.claims.methods.len(), 1);
    assert_eq!(
        manifest.claims.methods[0].kind,
        ty_plugin_protocol::MethodClaimKind::OnSubclassOfMatching {
            base_qualified_name: "example.Base".to_string(),
            method_name_pattern: "run_*".to_string(),
        }
    );
    assert_eq!(
        manifest.claims.attributes[0].kind,
        ty_plugin_protocol::AttributeClaimKind::Exact {
            owner_qualified_name: "example.Widget".to_string(),
            attribute_name: "state".to_string(),
            scope: ty_plugin_protocol::AttributeScope::Instance,
        }
    );
    assert_eq!(
        manifest.stub_overlays,
        vec![ty_plugin_protocol::StubOverlay {
            module: "example".to_string(),
            path: "stubs/example.pyi".to_string(),
            sha256: None,
        }]
    );

    // The filtered handler claims matching callees; the generic one catches the rest.
    let response = host
        .execute(&plugin_id, &call_request())
        .expect("filtered call executes");
    let PluginResponse::CallReturnPatch(patch) = response else {
        panic!("expected a call-return patch, got {response:?}");
    };
    assert_eq!(patch.return_type.expression, "example.Token");

    let other_call = PluginRequest::AdjustCallReturn(CallRequest {
        context: context(),
        callee: TypeExpr::expression("example.other"),
        receiver: None,
        arguments: Vec::new(),
        existing_signature: None,
        default_return_type: None,
        project_index: None,
    });
    let response = host
        .execute(&plugin_id, &other_call)
        .expect("unclaimed callee falls through to the generic handler");
    let PluginResponse::CallReturnPatch(patch) = response else {
        panic!("expected a call-return patch, got {response:?}");
    };
    assert_eq!(patch.return_type.expression, "example.Fallback");

    let response = host
        .execute(
            &plugin_id,
            &PluginRequest::BuildProjectIndex(ty_plugin_protocol::BuildProjectIndexRequest {
                context: ty_plugin_protocol::ProjectContext {
                    root: "/project".to_string(),
                    python_version: "3.13".to_string(),
                    platform: "linux".to_string(),
                    config: serde_json::Value::default(),
                },
                classes: Vec::new(),
                settings: Vec::new(),
                assignments: Vec::new(),
                functions: Vec::new(),
                previous_index_fingerprint: None,
            }),
        )
        .expect("project-index request executes");
    let PluginResponse::ProjectIndex(index) = response else {
        panic!("expected a project-index response, got {response:?}");
    };
    let ty_plugin_protocol::ContributionTarget::Instance { qualified_name } =
        &index.contributions[0].target
    else {
        panic!("expected an instance target, got {:?}", index.contributions);
    };
    assert_eq!(qualified_name, "example.Widget");
    let ty_plugin_protocol::ContributionPatch::Member(patch) = &index.contributions[0].patch else {
        panic!("expected a member patch, got {:?}", index.contributions);
    };
    assert_eq!(patch.name, "state");
    assert_eq!(
        index.virtual_types[0].shape,
        ty_plugin_protocol::VirtualTypeShape::NamedTuple {
            fields: vec![
                ty_plugin_protocol::VirtualTypeField {
                    name: "first".to_string(),
                    type_expr: TypeExpr::annotation("int"),
                    required: true,
                    read_only: false,
                },
                ty_plugin_protocol::VirtualTypeField {
                    name: "second".to_string(),
                    type_expr: TypeExpr::annotation("str"),
                    required: false,
                    read_only: false,
                },
            ],
        }
    );
}

#[test]
fn plugin_exception_surfaces_as_error_response() {
    const RAISING_PLUGIN: &str = r#"
@on_call_return
def adjust_runner(request):
    raise ValueError("plugin rejected the call")
"#;
    let manifest = ManifestBuilder::new("example.runner", "test", "0.0.0").build();
    let environment = PluginEnvironment::from_manifests(vec![manifest]).expect("manifest is valid");
    let mut runner = MontyRunner::new(MontyLimits::default()).expect("runner builds");
    runner
        .add_plugin("example.runner", RAISING_PLUGIN)
        .expect("plugin source compiles");
    let host = PluginHost::new(environment, runner);

    let response = host
        .execute("example.runner", &call_request())
        .expect("a raising hook returns a protocol error response");

    let PluginResponse::Error(error) = response else {
        panic!("expected an error response, got {response:?}");
    };
    assert!(
        error.message.contains("plugin rejected the call"),
        "unexpected error message: {}",
        error.message
    );
}

#[test]
fn invalid_python_fails_at_load() {
    let mut runner = MontyRunner::new(MontyLimits::default()).expect("runner builds");

    let error = runner
        .add_plugin("broken", "def broken(:\n")
        .expect_err("invalid Python is rejected");

    assert!(
        error.to_string().contains("compile"),
        "unexpected error: {error}"
    );
}

#[test]
fn non_string_handler_result_is_rejected() {
    const BAD_HANDLER: &str = r#"
def __ty_handle__(request_json):
    return 42
"#;
    let manifest = ManifestBuilder::new("bad.handler", "test", "0.0.0").build();
    let environment = PluginEnvironment::from_manifests(vec![manifest]).expect("manifest is valid");
    let plugin_id = "bad.handler".to_string();
    let mut runner = MontyRunner::new(MontyLimits::default()).expect("runner builds");
    runner
        .add_plugin(&plugin_id, BAD_HANDLER)
        .expect("plugin source compiles");
    let host = PluginHost::new(environment, runner);

    let error = host
        .execute(&plugin_id, &PluginRequest::Manifest)
        .expect_err("a non-string result is rejected");

    assert!(
        error.to_string().contains("non-string"),
        "unexpected error: {error}"
    );
}

#[test]
fn invalid_json_result_is_rejected() {
    const BAD_JSON: &str = r#"
def __ty_handle__(request_json):
    return "not json"
"#;
    let manifest = ManifestBuilder::new("bad.json", "test", "0.0.0").build();
    let environment = PluginEnvironment::from_manifests(vec![manifest]).expect("manifest is valid");
    let plugin_id = "bad.json".to_string();
    let mut runner = MontyRunner::new(MontyLimits::default()).expect("runner builds");
    runner
        .add_plugin(&plugin_id, BAD_JSON)
        .expect("plugin source compiles");
    let host = PluginHost::new(environment, runner);

    let error = host
        .execute(&plugin_id, &PluginRequest::Manifest)
        .expect_err("invalid JSON is rejected");

    assert!(
        error.to_string().contains("invalid response JSON"),
        "unexpected error: {error}"
    );
}

#[test]
fn oversized_response_is_rejected() {
    let limits = MontyLimits {
        max_response_bytes: 4,
        ..MontyLimits::default()
    };
    let host = example_host(limits);

    let error = host
        .execute("example.runner", &PluginRequest::Manifest)
        .expect_err("an oversized response is rejected");

    assert!(
        error.to_string().contains("size limit"),
        "unexpected error: {error}"
    );
}

#[test]
fn sandbox_denies_filesystem_access() {
    const ESCAPING_PLUGIN: &str = r#"
@on_call_return
def probe(request):
    return call_return_patch(type_expr(open("/etc/passwd").read()))
"#;
    let manifest = ManifestBuilder::new("sandbox.probe", "test", "0.0.0").build();
    let environment = PluginEnvironment::from_manifests(vec![manifest]).expect("manifest is valid");
    let plugin_id = "sandbox.probe".to_string();
    let mut runner = MontyRunner::new(MontyLimits::default()).expect("runner builds");
    runner
        .add_plugin(&plugin_id, ESCAPING_PLUGIN)
        .expect("plugin source compiles");
    let host = PluginHost::new(environment, runner);

    let error = host
        .execute(&plugin_id, &call_request())
        .expect_err("filesystem access is denied");

    assert!(
        error.to_string().contains("open"),
        "unexpected error: {error}"
    );
}

#[test]
fn sandboxed_time_and_random_are_deterministic() {
    const CLOCK_PLUGIN: &str = r#"
import time
import random

@on_call_return
def adjust_runner(request):
    observed = time.time() + time.process_time() + random.random()
    return call_return_patch(type_expr(str(observed)))
"#;
    let manifest = ManifestBuilder::new("example.clock", "test", "0.0.0").build();
    let environment = PluginEnvironment::from_manifests(vec![manifest]).expect("manifest is valid");
    let plugin_id = "example.clock".to_string();
    let mut runner = MontyRunner::new(MontyLimits::default()).expect("runner builds");
    runner
        .add_plugin(&plugin_id, CLOCK_PLUGIN)
        .expect("plugin source compiles");
    let host = PluginHost::new(environment, runner);

    let response = host
        .execute(&plugin_id, &call_request())
        .expect("call-return request executes");
    let PluginResponse::CallReturnPatch(patch) = response else {
        panic!("expected a call-return patch, got {response:?}");
    };

    // The fixed os policy pins `time.time`/`process_time` to zero and seeds `random`
    // deterministically, so the summed value is a constant across runs.
    let first = patch.return_type.expression;
    let response = host
        .execute(&plugin_id, &call_request())
        .expect("second call executes");
    let PluginResponse::CallReturnPatch(patch) = response else {
        panic!("expected a call-return patch, got {response:?}");
    };
    assert_eq!(patch.return_type.expression, first);
}

#[test]
fn plugin_print_output_does_not_corrupt_responses() {
    const CHATTY_PLUGIN: &str = r#"
@on_call_return
def adjust_runner(request):
    print("handling", request["kind"])
    return call_return_patch(type_expr("example.Token", mode="annotation"))
"#;
    let manifest = ManifestBuilder::new("example.chatty", "test", "0.0.0").build();
    let environment = PluginEnvironment::from_manifests(vec![manifest]).expect("manifest is valid");
    let plugin_id = "example.chatty".to_string();
    let mut runner = MontyRunner::new(MontyLimits::default()).expect("runner builds");
    runner
        .add_plugin(&plugin_id, CHATTY_PLUGIN)
        .expect("plugin source compiles");
    let host = PluginHost::new(environment, runner);

    let response = host
        .execute(&plugin_id, &call_request())
        .expect("call-return request executes");
    let PluginResponse::CallReturnPatch(patch) = response else {
        panic!("expected a call-return patch, got {response:?}");
    };
    assert_eq!(patch.return_type.expression, "example.Token");
}

#[test]
fn looping_plugin_is_stopped_by_limits() {
    const LOOPER: &str = r#"
@on_call_return
def adjust_runner(request):
    while True:
        pass
"#;
    let manifest = ManifestBuilder::new("looper", "test", "0.0.0").build();
    let environment = PluginEnvironment::from_manifests(vec![manifest]).expect("manifest is valid");
    let plugin_id = "looper".to_string();
    let limits = MontyLimits {
        max_feed_duration: std::time::Duration::from_millis(100),
        ..MontyLimits::default()
    };
    let mut runner = MontyRunner::new(limits).expect("runner builds");
    runner
        .add_plugin(&plugin_id, LOOPER)
        .expect("plugin source compiles");
    let host = PluginHost::new(environment, runner);

    let error = host
        .execute(&plugin_id, &call_request())
        .expect_err("a looping plugin is stopped");

    assert!(
        error.to_string().contains("ime") || error.to_string().contains("loop"),
        "unexpected error: {error}"
    );
}

#[test]
fn recursion_is_stopped_by_limits() {
    let host = source_host(
        MontyLimits {
            max_recursion_depth: 50,
            ..MontyLimits::default()
        },
        r#"
def recurse():
    return recurse()

def __ty_handle__(request_json):
    if json.loads(request_json)["kind"] == "manifest":
        return json.dumps(no_change())
    return recurse()
"#,
    );
    let error = host
        .execute("example.runner", &call_request())
        .expect_err("recursion is bounded");
    assert!(
        error.to_string().contains("RecursionError"),
        "unexpected error: {error}"
    );
    host.execute("example.runner", &PluginRequest::Manifest)
        .expect("interpreter remains usable");
}

#[test]
fn python_exception_does_not_invalidate_interpreter() {
    let host = source_host(
        MontyLimits::default(),
        r#"
def __ty_handle__(request_json):
    if json.loads(request_json)["kind"] == "manifest":
        return json.dumps(no_change())
    raise ValueError("invalid plugin input")
"#,
    );
    let error = host
        .execute("example.runner", &call_request())
        .expect_err("Python exception is reported");
    assert!(
        error.to_string().contains("ValueError"),
        "unexpected error: {error}"
    );
    host.execute("example.runner", &PluginRequest::Manifest)
        .expect("interpreter remains usable");
}

#[test]
fn state_hook_round_trips_through_embedded_monty() {
    let host = source_host(
        MontyLimits::default(),
        r#"
set_manifest(manifest(id="example.runner", name="State", version="0.1.0",
    capabilities=capabilities(call_state=True),
    claims=claims(functions=[symbol_claim("example.runner")],
        constructors=[class_claim_subclass_of("example.Record")])))

@on_call_state_of("example.runner")
def state(request):
    return call_state_patch(receiver_members={"key": type_expr("int", mode="annotation")},
        preserves_other_objects=True)
"#,
    );
    let PluginRequest::AdjustCallReturn(request) = call_request() else {
        return;
    };
    let response = host
        .execute("example.runner", &PluginRequest::AdjustCallState(request))
        .expect("state hook");
    let PluginResponse::CallStatePatch(patch) = response else {
        panic!("expected state patch");
    };
    assert_eq!(patch.receiver_members["key"].expression, "int");
    assert!(patch.preserves_other_objects);
    assert!(!patch.fresh_result);
}
