use ty_plugin_protocol::{
    AnalyzeClassRequest, ArgumentKind, ArgumentSummary, AssignedValueSummary,
    BuildProjectIndexRequest, CallOrSymbolSummary, CallRequest, CallValueSummary,
    CallableSignature, ClassClaim, ClassPatch, ClassSummary, FieldPatch, FieldSummary,
    LiteralValue, MemberAccessPatch, MemberPatchMode, MutationOperation, MutationRequest,
    MutationResponse, Parameter, ParameterKind, PluginCapabilities, PluginClaims, PluginManifest,
    PluginRequest, PluginResponse, ProjectContext, ProjectIndexResponse, ProtocolVersion,
    ReceiverSummary, RuntimeSpec, SemanticContext, SettingsModuleSummary, SymbolRef, SymbolSource,
    TypeExpr, TypeSnapshot, TypeSnapshotField, VersionReq,
};

#[test]
fn class_batches_accept_only_class_messages_and_preserve_item_wire_format() {
    let item = serde_json::json!({"kind":"analyze-class","context":{"module":"demo","file-path":"demo.py","python-version":"3.13","platform":"linux","config":{},"speculative":false},"class":{"qualified-name":"demo.Row"},"project-index":{"opaque":{"kind":"analyze-classes"}}});
    let individual: PluginRequest = serde_json::from_value(item).unwrap();
    let item = serde_json::to_value(individual).unwrap();
    let batch = serde_json::json!({"kind":"analyze-classes","requests":[item.clone(),item]});
    let decoded: PluginRequest = serde_json::from_value(batch.clone()).unwrap();
    assert_eq!(serde_json::to_value(decoded).unwrap(), batch);
    let response = serde_json::json!({"kind":"class-batch","responses":[{"kind":"class-patch"},{"kind":"no-change"},{"kind":"error","message":"expected failure"}]});
    let decoded: PluginResponse = serde_json::from_value(response.clone()).unwrap();
    assert_eq!(serde_json::to_value(decoded).unwrap(), response);
    for wrong in [serde_json::json!({"kind":"manifest"}), batch] {
        assert!(
            serde_json::from_value::<PluginRequest>(
                serde_json::json!({"kind":"analyze-classes","requests":[wrong]})
            )
            .is_err()
        );
    }
    for wrong in [
        serde_json::json!({"kind":"project-index","plugin-index":{}}),
        response,
    ] {
        assert!(
            serde_json::from_value::<PluginResponse>(
                serde_json::json!({"kind":"class-batch","responses":[wrong]})
            )
            .is_err()
        );
    }
    assert!(
        !ProtocolVersion { major: 0, minor: 8 }.supports(ProtocolVersion { major: 0, minor: 9 })
    );
}

#[test]
fn class_scoped_indexes_preserve_opaque_json_and_old_wire_messages() {
    let old = serde_json::json!({"kind":"project-index","plugin-index":{"all":true}});
    let response: PluginResponse = serde_json::from_value(old.clone()).unwrap();
    assert_eq!(serde_json::to_value(response).unwrap(), old);
    let scoped = serde_json::json!({
        "kind":"project-index", "plugin-index":{"all":true},
        "class-indexes":{
            "app.Row":{"qualified-name":"opaque.Name","expression":"opaque.Type","mode":"annotation"},
            "app.Empty":null, "app.Array":[null,0,"text"], "app.String":"opaque"
        }
    });
    let response: PluginResponse = serde_json::from_value(scoped.clone()).unwrap();
    assert_eq!(serde_json::to_value(response).unwrap(), scoped);
    assert!(
        !ProtocolVersion { major: 0, minor: 7 }.supports(ProtocolVersion { major: 0, minor: 8 })
    );
    assert!(ProtocolVersion::CURRENT.supports(ProtocolVersion { major: 0, minor: 7 }));
}

#[test]
fn serializes_manifest() {
    let manifest = PluginManifest {
        id: "example.model".to_string(),
        name: "Example model plugin".to_string(),
        version: "0.1.0".to_string(),
        protocol_version: ProtocolVersion { major: 0, minor: 1 },
        ty_compatibility: VersionReq {
            requirement: ">=0.0.0".to_string(),
        },
        runtime: RuntimeSpec::Mock,
        capabilities: PluginCapabilities {
            class_transform: true,
            project_index: true,
            settings_data: true,
            ..PluginCapabilities::default()
        },
        claims: PluginClaims {
            classes: vec![ClassClaim::subclass_of("example.Model")],
            settings: vec![ty_plugin_protocol::SettingsClaim {
                module: "app.settings".to_string(),
                config_key: None,
            }],
            ..PluginClaims::default()
        },
        config_schema: None,
        default_config: None,
        stub_overlays: Vec::new(),
    };

    insta::assert_json_snapshot!(manifest, @r#"
    {
      "id": "example.model",
      "name": "Example model plugin",
      "version": "0.1.0",
      "protocol-version": {
        "major": 0,
        "minor": 1
      },
      "ty-compatibility": {
        "requirement": ">=0.0.0"
      },
      "runtime": {
        "kind": "mock"
      },
      "capabilities": {
        "stub-overlays": false,
        "class-transform": true,
        "class-member": false,
        "instance-member": false,
        "call-signature": false,
        "call-return": false,
        "call-state": false,
        "additional-dependencies": false,
        "project-index": true,
        "cross-symbol-contributions": false,
        "settings-data": true,
        "virtual-types": false,
        "mutation-validation": false
      },
      "claims": {
        "classes": [
          {
            "kind": "subclass-of",
            "base-qualified-name": "example.Model"
          }
        ],
        "settings": [
          {
            "module": "app.settings"
          }
        ]
      }
    }
    "#);
}

#[test]
fn serializes_monty_runtime_spec() {
    let spec = RuntimeSpec::Monty(ty_plugin_protocol::MontyRuntimeSpec {
        artifact: "plugins/example.py".to_string(),
        sha256: Some("abc123".to_string()),
    });

    insta::assert_json_snapshot!(spec, @r#"
    {
      "kind": "monty",
      "artifact": "plugins/example.py",
      "sha256": "abc123"
    }
    "#);

    let json = serde_json::to_string(&spec).expect("serialize spec");
    let restored: RuntimeSpec = serde_json::from_str(&json).expect("deserialize spec");
    assert_eq!(restored, spec);
}

#[test]
fn serializes_method_pattern_claim() {
    let claim =
        ty_plugin_protocol::MethodClaim::on_subclass_of_matching("app.workers.Runner", "run_*");

    insta::assert_json_snapshot!(claim, @r#"
    {
      "kind": "on-subclass-of-matching",
      "base-qualified-name": "app.workers.Runner",
      "method-name-pattern": "run_*"
    }
    "#);

    let json = serde_json::to_string(&claim).expect("serialize claim");
    let restored: ty_plugin_protocol::MethodClaim =
        serde_json::from_str(&json).expect("deserialize claim");
    assert_eq!(restored, claim);
}

#[test]
fn serializes_class_transform_request() {
    let request = PluginRequest::AnalyzeClass(AnalyzeClassRequest {
        context: context(),
        class: ClassSummary {
            qualified_name: "app.User".to_string(),
            bases: vec![TypeExpr::expression("example.Model")],
            decorators: Vec::new(),
            metaclass: None,
            fields: vec![FieldSummary {
                name: "name".to_string(),
                annotation: Some(TypeExpr::expression("str")),
                assigned_value: Some(AssignedValueSummary::Call(CallValueSummary {
                    callee: SymbolRef {
                        qualified_name: "minidjango.CharField".to_string(),
                    },
                    receiver: None,
                    arguments: vec![ArgumentSummary {
                        name: Some("max_length".to_string()),
                        kind: ArgumentKind::Keyword,
                        type_expr: Some(TypeExpr::annotation("int")),
                        value: LiteralValue::Int { value: 100 },
                        source: None,
                    }],
                    return_type: Some(TypeExpr::annotation("minidjango.CharField[str]")),
                })),
                inferred_type: Some(TypeExpr::annotation("minidjango.CharField[str]")),
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

    insta::assert_json_snapshot!(request, @r#"
    {
      "kind": "analyze-class",
      "context": {
        "module": "app",
        "file-path": "/project/app.py",
        "python-version": "3.13",
        "platform": "linux",
        "speculative": false
      },
      "class": {
        "qualified-name": "app.User",
        "bases": [
          {
            "expression": "example.Model",
            "mode": "expression"
          }
        ],
        "fields": [
          {
            "name": "name",
            "annotation": {
              "expression": "str",
              "mode": "expression"
            },
            "assigned-value": {
              "kind": "call",
              "callee": {
                "qualified-name": "minidjango.CharField"
              },
              "arguments": [
                {
                  "name": "max_length",
                  "kind": "keyword",
                  "type-expr": {
                    "expression": "int",
                    "mode": "annotation"
                  },
                  "value": {
                    "kind": "int",
                    "value": 100
                  }
                }
              ],
              "return-type": {
                "expression": "minidjango.CharField[str]",
                "mode": "annotation"
              }
            },
            "inferred-type": {
              "expression": "minidjango.CharField[str]",
              "mode": "annotation"
            },
            "has-default": false
          }
        ]
      }
    }
    "#);
}

#[test]
fn serializes_class_patch_response() {
    let response = PluginResponse::ClassPatch(ClassPatch {
        fields: vec![FieldPatch {
            name: "name".to_string(),
            mode: MemberPatchMode::FillOnMiss,
            descriptor: Some(MemberAccessPatch::Descriptor {
                class_type: Some(TypeExpr::annotation("minidjango.CharField[str]")),
                instance_get_type: TypeExpr::annotation("str"),
                instance_set_type: Some(TypeExpr::annotation("str")),
            }),
            instance_get_type: TypeExpr::expression("str"),
            instance_set_type: Some(TypeExpr::annotation("str")),
            constructor_parameter: Some(Parameter {
                name: Some("name".to_string()),
                kind: ParameterKind::KeywordOnly,
                type_expr: Some(TypeExpr::expression("str")),
                required: true,
            }),
            has_default: false,
        }],
        class_members: Vec::new(),
        instance_members: Vec::new(),
        constructor: Some(CallableSignature {
            parameters: vec![Parameter {
                name: Some("name".to_string()),
                kind: ParameterKind::KeywordOnly,
                type_expr: Some(TypeExpr::expression("str")),
                required: true,
            }],
            return_type: TypeExpr::expression("Self"),
        }),
        diagnostics: Vec::new(),
    });

    insta::assert_json_snapshot!(response, @r#"
    {
      "kind": "class-patch",
      "fields": [
        {
          "name": "name",
          "mode": "fill-on-miss",
          "descriptor": {
            "kind": "descriptor",
            "class-type": {
              "expression": "minidjango.CharField[str]",
              "mode": "annotation"
            },
            "instance-get-type": {
              "expression": "str",
              "mode": "annotation"
            },
            "instance-set-type": {
              "expression": "str",
              "mode": "annotation"
            }
          },
          "instance-get-type": {
            "expression": "str",
            "mode": "expression"
          },
          "instance-set-type": {
            "expression": "str",
            "mode": "annotation"
          },
          "constructor-parameter": {
            "name": "name",
            "kind": "keyword-only",
            "type-expr": {
              "expression": "str",
              "mode": "expression"
            },
            "required": true
          },
          "has-default": false
        }
      ],
      "constructor": {
        "parameters": [
          {
            "name": "name",
            "kind": "keyword-only",
            "type-expr": {
              "expression": "str",
              "mode": "expression"
            },
            "required": true
          }
        ],
        "return-type": {
          "expression": "Self",
          "mode": "expression"
        }
      }
    }
    "#);
}

#[test]
fn serializes_project_index_request_and_response() {
    let request = PluginRequest::BuildProjectIndex(BuildProjectIndexRequest {
        context: ProjectContext {
            root: "/project".to_string(),
            python_version: "3.13".to_string(),
            platform: "linux".to_string(),
            config: serde_json::json!({ "strict_settings": true }),
        },
        classes: vec![ClassSummary {
            qualified_name: "app.Book".to_string(),
            bases: vec![TypeExpr::annotation("minidjango.Model")],
            decorators: Vec::new(),
            metaclass: None,
            fields: vec![FieldSummary {
                name: "author".to_string(),
                annotation: None,
                assigned_value: Some(AssignedValueSummary::Call(CallValueSummary {
                    callee: SymbolRef {
                        qualified_name: "minidjango.ForeignKey".to_string(),
                    },
                    receiver: None,
                    arguments: vec![ArgumentSummary {
                        name: None,
                        kind: ArgumentKind::Positional,
                        type_expr: Some(TypeExpr::annotation("type[app.Author]")),
                        value: LiteralValue::ClassRef(SymbolRef {
                            qualified_name: "app.Author".to_string(),
                        }),
                        source: None,
                    }],
                    return_type: None,
                })),
                inferred_type: None,
                has_default: false,
                source: SymbolSource::default(),
            }],
            methods: Vec::new(),
            nested_classes: Vec::new(),
            class_constants: Vec::new(),
            source: SymbolSource::default(),
        }],
        settings: vec![SettingsModuleSummary {
            module: "app.settings".to_string(),
            values: Vec::new(),
            dependencies: Vec::new(),
            diagnostics: Vec::new(),
            source: SymbolSource::default(),
        }],
        assignments: Vec::new(),
        functions: vec![ty_plugin_protocol::FunctionSummary {
            qualified_name: "app.jobs.handle_alpha".to_string(),
            decorators: vec![CallOrSymbolSummary::Call(CallValueSummary {
                callee: SymbolRef {
                    qualified_name: "app.marks.mark".to_string(),
                },
                receiver: None,
                arguments: vec![
                    ArgumentSummary {
                        name: None,
                        kind: ArgumentKind::Positional,
                        type_expr: None,
                        value: LiteralValue::ClassRef(SymbolRef {
                            qualified_name: "app.Alpha".to_string(),
                        }),
                        source: None,
                    },
                    ArgumentSummary {
                        name: None,
                        kind: ArgumentKind::Positional,
                        type_expr: None,
                        value: LiteralValue::ClassRef(SymbolRef {
                            qualified_name: "app.Beta".to_string(),
                        }),
                        source: None,
                    },
                ],
                return_type: None,
            })],
            inferred_type: Some(TypeExpr::annotation("app.Token[app.Alpha, app.Beta]")),
            source: SymbolSource::default(),
        }],
        previous_index_fingerprint: None,
    });

    insta::assert_json_snapshot!(request, @r#"
    {
      "kind": "build-project-index",
      "context": {
        "root": "/project",
        "python-version": "3.13",
        "platform": "linux",
        "config": {
          "strict_settings": true
        }
      },
      "classes": [
        {
          "qualified-name": "app.Book",
          "bases": [
            {
              "expression": "minidjango.Model",
              "mode": "annotation"
            }
          ],
          "fields": [
            {
              "name": "author",
              "assigned-value": {
                "kind": "call",
                "callee": {
                  "qualified-name": "minidjango.ForeignKey"
                },
                "arguments": [
                  {
                    "kind": "positional",
                    "type-expr": {
                      "expression": "type[app.Author]",
                      "mode": "annotation"
                    },
                    "value": {
                      "kind": "class-ref",
                      "qualified-name": "app.Author"
                    }
                  }
                ]
              },
              "has-default": false
            }
          ]
        }
      ],
      "settings": [
        {
          "module": "app.settings"
        }
      ],
      "functions": [
        {
          "qualified-name": "app.jobs.handle_alpha",
          "decorators": [
            {
              "kind": "call",
              "callee": {
                "qualified-name": "app.marks.mark"
              },
              "arguments": [
                {
                  "kind": "positional",
                  "value": {
                    "kind": "class-ref",
                    "qualified-name": "app.Alpha"
                  }
                },
                {
                  "kind": "positional",
                  "value": {
                    "kind": "class-ref",
                    "qualified-name": "app.Beta"
                  }
                }
              ]
            }
          ],
          "inferred-type": {
            "expression": "app.Token[app.Alpha, app.Beta]",
            "mode": "annotation"
          }
        }
      ]
    }
    "#);

    let response = PluginResponse::ProjectIndex(ProjectIndexResponse {
        class_indexes: std::collections::BTreeMap::new(),
        plugin_index: serde_json::json!({ "models": ["app.Book"] }),
        contributions: Vec::new(),
        virtual_types: Vec::new(),
        dependencies: Vec::new(),
        diagnostics: Vec::new(),
    });

    insta::assert_json_snapshot!(response, @r#"
    {
      "kind": "project-index",
      "plugin-index": {
        "models": [
          "app.Book"
        ]
      }
    }
    "#);
}

#[test]
fn serializes_receiver_aware_call_request() {
    let request = PluginRequest::AdjustCallReturn(CallRequest {
        context: context(),
        callee: TypeExpr::annotation("minidjango.Manager.filter"),
        receiver: Some(ReceiverSummary {
            type_expr: TypeExpr::annotation("minidjango.Manager[app.Book]"),
            nominal_class: Some("minidjango.Manager".to_string()),
            generic_arguments: vec![TypeExpr::annotation("app.Book")],
            plugin_metadata: serde_json::json!({ "model": "app.Book" }),
        }),
        arguments: vec![ArgumentSummary {
            name: Some("title".to_string()),
            kind: ArgumentKind::Keyword,
            type_expr: Some(TypeExpr::annotation("str")),
            value: LiteralValue::Str {
                value: "x".to_string(),
            },
            source: None,
        }],
        existing_signature: None,
        default_return_type: Some(TypeExpr::annotation("minidjango.QuerySet[app.Book]")),
        project_index: Some(serde_json::json!({ "models": ["app.Book"] })),
    });

    insta::assert_json_snapshot!(request, @r#"
    {
      "kind": "adjust-call-return",
      "context": {
        "module": "app",
        "file-path": "/project/app.py",
        "python-version": "3.13",
        "platform": "linux",
        "speculative": false
      },
      "callee": {
        "expression": "minidjango.Manager.filter",
        "mode": "annotation"
      },
      "receiver": {
        "type-expr": {
          "expression": "minidjango.Manager[app.Book]",
          "mode": "annotation"
        },
        "nominal-class": "minidjango.Manager",
        "generic-arguments": [
          {
            "expression": "app.Book",
            "mode": "annotation"
          }
        ],
        "plugin-metadata": {
          "model": "app.Book"
        }
      },
      "arguments": [
        {
          "name": "title",
          "kind": "keyword",
          "type-expr": {
            "expression": "str",
            "mode": "annotation"
          },
          "value": {
            "kind": "str",
            "value": "x"
          }
        }
      ],
      "default-return-type": {
        "expression": "minidjango.QuerySet[app.Book]",
        "mode": "annotation"
      },
      "project-index": {
        "models": [
          "app.Book"
        ]
      }
    }
    "#);
}

#[test]
fn structural_type_snapshot_round_trips() {
    let snapshot = TypeSnapshot::Nominal {
        qualified_name: "django.db.models.query.QuerySet".to_string(),
        arguments: vec![
            TypeSnapshot::expression(&TypeExpr::annotation("app.Book")),
            TypeSnapshot::TypedDict {
                fields: vec![TypeSnapshotField {
                    name: "title".to_string(),
                    type_snapshot: TypeSnapshot::expression(&TypeExpr::annotation("str")),
                    required: true,
                    read_only: false,
                }],
                extra_items: None,
                closed: true,
            },
        ],
    };
    let type_expr =
        TypeExpr::annotation("QuerySet[Book, TypedDict(...)]").with_snapshot(snapshot.clone());

    let json = serde_json::to_string(&type_expr).expect("serialize type snapshot");
    let restored: TypeExpr = serde_json::from_str(&json).expect("deserialize type snapshot");

    assert_eq!(restored.snapshot.as_deref(), Some(&snapshot));
}

#[test]
fn mutation_request_and_response_round_trip() {
    let request = PluginRequest::ValidateMutation(Box::new(MutationRequest {
        context: context(),
        operation: MutationOperation::ItemSet,
        receiver: TypeExpr::annotation("django.http.request.QueryDict"),
        key: Some(ArgumentSummary {
            name: None,
            kind: ArgumentKind::Positional,
            type_expr: Some(TypeExpr::annotation("str")),
            value: LiteralValue::Str {
                value: "name".to_string(),
            },
            source: None,
        }),
        value: Some(ArgumentSummary {
            name: None,
            kind: ArgumentKind::Positional,
            type_expr: Some(TypeExpr::annotation("str")),
            value: LiteralValue::Str {
                value: "Ada".to_string(),
            },
            source: None,
        }),
        source: SymbolSource::default(),
        project_index: None,
    }));
    let request_json = serde_json::to_string(&request).expect("serialize mutation request");
    let restored_request: PluginRequest =
        serde_json::from_str(&request_json).expect("deserialize mutation request");
    assert_eq!(restored_request, request);

    let response = PluginResponse::MutationDiagnostics(MutationResponse {
        diagnostics: Vec::new(),
    });
    let response_json = serde_json::to_string(&response).expect("serialize mutation response");
    let restored_response: PluginResponse =
        serde_json::from_str(&response_json).expect("deserialize mutation response");
    assert_eq!(restored_response, response);
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
