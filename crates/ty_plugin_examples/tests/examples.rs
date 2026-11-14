//! Behavioral tests for the example plugins, driven only through the SDK.

use ty_plugin_examples::{
    FieldCallReturnPlugin, MiniDjangoPlugin, ModelClassTransformPlugin, StubOverlayPlugin,
};
use ty_plugin_examples::{call_return, class_transform, minidjango, stub_overlay};
use ty_plugin_sdk::Plugin;
use ty_plugin_sdk::protocol::{
    AnalyzeClassRequest, ArgumentKind, ArgumentSummary, AssignedValueSummary,
    BuildProjectIndexRequest, CallRequest, CallValueSummary, ClassClaimKind, ClassSummary,
    ContributionPatch, ContributionTarget, FieldSummary, LiteralValue, MethodClaimKind,
    ParameterKind, PluginResponse, ProjectContext, ReceiverSummary, SemanticContext,
    SettingValueSummary, SettingsModuleSummary, SymbolRef, SymbolSource, TextPosition, TypeExpr,
    VirtualTypeShape,
};
use ty_plugin_sdk::serde_json::{Value, json};

fn context() -> SemanticContext {
    SemanticContext {
        module: "app".to_string(),
        file_path: "/project/app.py".to_string(),
        python_version: "3.13".to_string(),
        platform: "linux".to_string(),
        config: Value::Null,
        speculative: false,
    }
}

#[test]
fn stub_overlay_declares_overlay_in_manifest() {
    let manifest = StubOverlayPlugin.manifest();

    assert!(manifest.capabilities.stub_overlays);
    assert_eq!(manifest.stub_overlays.len(), 1);
    assert_eq!(
        manifest.stub_overlays[0].module,
        stub_overlay::OVERLAY_MODULE
    );
    assert_eq!(manifest.stub_overlays[0].path, stub_overlay::OVERLAY_PATH);
    // A manifest-only plugin has nothing to say to semantic hook requests.
    assert_eq!(
        StubOverlayPlugin.analyze_class(&analyze_request(vec![])),
        PluginResponse::NoChange
    );
}

fn analyze_request(fields: Vec<FieldSummary>) -> AnalyzeClassRequest {
    AnalyzeClassRequest {
        context: context(),
        class: ClassSummary {
            qualified_name: "app.User".to_string(),
            bases: vec![TypeExpr::expression(class_transform::MODEL_BASE)],
            decorators: Vec::new(),
            metaclass: None,
            fields,
            methods: Vec::new(),
            nested_classes: Vec::new(),
            class_constants: Vec::new(),
            source: SymbolSource::default(),
        },
        project_index: None,
    }
}

fn minidjango_analyze_request(fields: Vec<FieldSummary>) -> AnalyzeClassRequest {
    AnalyzeClassRequest {
        context: context(),
        class: ClassSummary {
            qualified_name: "app.Book".to_string(),
            bases: vec![TypeExpr::expression(minidjango::MODEL_BASE)],
            decorators: Vec::new(),
            metaclass: None,
            fields,
            methods: Vec::new(),
            nested_classes: Vec::new(),
            class_constants: Vec::new(),
            source: SymbolSource::default(),
        },
        project_index: None,
    }
}

fn call_field(name: &str, callee: &str, arguments: Vec<ArgumentSummary>) -> FieldSummary {
    FieldSummary {
        name: name.to_string(),
        annotation: None,
        assigned_value: Some(AssignedValueSummary::Call(CallValueSummary {
            callee: SymbolRef {
                qualified_name: callee.to_string(),
            },
            receiver: None,
            arguments,
            return_type: None,
        })),
        inferred_type: None,
        has_default: false,
        source: SymbolSource::default(),
    }
}

fn keyword_bool(name: &str, value: bool) -> ArgumentSummary {
    ArgumentSummary {
        name: Some(name.to_string()),
        kind: ArgumentKind::Keyword,
        type_expr: Some(TypeExpr::annotation("bool")),
        value: LiteralValue::Bool { value },
        source: None,
    }
}

fn keyword_int(name: &str, value: i64) -> ArgumentSummary {
    ArgumentSummary {
        name: Some(name.to_string()),
        kind: ArgumentKind::Keyword,
        type_expr: Some(TypeExpr::annotation("int")),
        value: LiteralValue::Int { value },
        source: None,
    }
}

fn keyword_tuple(name: &str, items: Vec<LiteralValue>) -> ArgumentSummary {
    ArgumentSummary {
        name: Some(name.to_string()),
        kind: ArgumentKind::Keyword,
        type_expr: None,
        value: LiteralValue::Tuple { items },
        source: None,
    }
}

fn keyword_list(name: &str, items: Vec<LiteralValue>) -> ArgumentSummary {
    ArgumentSummary {
        name: Some(name.to_string()),
        kind: ArgumentKind::Keyword,
        type_expr: None,
        value: LiteralValue::List { items },
        source: None,
    }
}

fn keyword_value(name: &str, value: LiteralValue) -> ArgumentSummary {
    ArgumentSummary {
        name: Some(name.to_string()),
        kind: ArgumentKind::Keyword,
        type_expr: None,
        value,
        source: None,
    }
}

fn class_arg(qualified_name: &str) -> ArgumentSummary {
    ArgumentSummary {
        name: None,
        kind: ArgumentKind::Positional,
        type_expr: Some(TypeExpr::annotation(qualified_name)),
        value: LiteralValue::ClassRef(SymbolRef {
            qualified_name: qualified_name.to_string(),
        }),
        source: None,
    }
}

fn positional_str(value: &str) -> ArgumentSummary {
    ArgumentSummary {
        name: None,
        kind: ArgumentKind::Positional,
        type_expr: Some(TypeExpr::annotation("str")),
        value: LiteralValue::Str {
            value: value.to_string(),
        },
        source: None,
    }
}

fn positional_int(value: i64) -> ArgumentSummary {
    ArgumentSummary {
        name: None,
        kind: ArgumentKind::Positional,
        type_expr: Some(TypeExpr::annotation("int")),
        value: LiteralValue::Int { value },
        source: None,
    }
}

fn setting_ref(qualified_name: &str) -> ArgumentSummary {
    ArgumentSummary {
        name: None,
        kind: ArgumentKind::Positional,
        type_expr: None,
        value: LiteralValue::EnumRef(SymbolRef {
            qualified_name: qualified_name.to_string(),
        }),
        source: None,
    }
}

fn source(path: &str) -> SymbolSource {
    SymbolSource {
        file_path: Some(path.to_string()),
        start: Some(TextPosition { line: 1, column: 1 }),
        end: Some(TextPosition { line: 1, column: 2 }),
        ..SymbolSource::default()
    }
}

fn with_arg_source(mut argument: ArgumentSummary) -> ArgumentSummary {
    argument.source = Some(source("/project/app.py"));
    argument
}

fn with_field_source(mut field: FieldSummary) -> FieldSummary {
    field.source = source("/project/app.py");
    field
}

fn keyword_str(name: &str, value: &str) -> ArgumentSummary {
    ArgumentSummary {
        name: Some(name.to_string()),
        kind: ArgumentKind::Keyword,
        type_expr: Some(TypeExpr::annotation("str")),
        value: LiteralValue::Str {
            value: value.to_string(),
        },
        source: None,
    }
}

fn annotation_field(name: &str, annotation: Option<TypeExpr>) -> FieldSummary {
    FieldSummary {
        name: name.to_string(),
        annotation,
        assigned_value: None,
        inferred_type: None,
        has_default: false,
        source: SymbolSource::default(),
    }
}

fn settings_module(module: &str, name: &str, value: &str) -> SettingsModuleSummary {
    SettingsModuleSummary {
        module: module.to_string(),
        values: vec![SettingValueSummary {
            name: name.to_string(),
            value: LiteralValue::Str {
                value: value.to_string(),
            },
            source: SymbolSource::default(),
        }],
        dependencies: Vec::new(),
        diagnostics: Vec::new(),
        source: SymbolSource::default(),
    }
}

fn mixed_settings_module() -> SettingsModuleSummary {
    SettingsModuleSummary {
        module: "minidjango_settings".to_string(),
        values: vec![
            SettingValueSummary {
                name: "IGNORED_INT".to_string(),
                value: LiteralValue::Int { value: 1 },
                source: SymbolSource::default(),
            },
            SettingValueSummary {
                name: "AUTH_USER_MODEL".to_string(),
                value: LiteralValue::Str {
                    value: "accounts.User".to_string(),
                },
                source: SymbolSource::default(),
            },
        ],
        dependencies: Vec::new(),
        diagnostics: Vec::new(),
        source: SymbolSource::default(),
    }
}

#[test]
fn class_transform_synthesizes_fields_and_constructor() {
    let request = analyze_request(vec![
        FieldSummary {
            name: "name".to_string(),
            annotation: Some(TypeExpr::annotation("str")),
            assigned_value: None,
            inferred_type: Some(TypeExpr::annotation("str")),
            has_default: false,
            source: SymbolSource::default(),
        },
        FieldSummary {
            name: "age".to_string(),
            annotation: Some(TypeExpr::annotation("int")),
            assigned_value: None,
            inferred_type: Some(TypeExpr::annotation("int")),
            has_default: true,
            source: SymbolSource::default(),
        },
        FieldSummary {
            name: "ignored".to_string(),
            annotation: None,
            assigned_value: None,
            inferred_type: None,
            has_default: false,
            source: SymbolSource::default(),
        },
    ]);

    let PluginResponse::ClassPatch(patch) = ModelClassTransformPlugin.analyze_class(&request)
    else {
        panic!("expected a class patch");
    };

    assert_eq!(patch.fields.len(), 2);
    assert!(
        patch
            .fields
            .iter()
            .all(|field| field.constructor_parameter.is_some())
    );

    let constructor = patch.constructor.expect("constructor signature");
    assert_eq!(constructor.parameters.len(), 2);
    assert!(
        constructor
            .parameters
            .iter()
            .all(|parameter| parameter.kind == ParameterKind::KeywordOnly)
    );

    let name = &constructor.parameters[0];
    assert_eq!(name.name.as_deref(), Some("name"));
    assert!(name.required);

    let age = &constructor.parameters[1];
    assert_eq!(age.name.as_deref(), Some("age"));
    // The field carried a default, so its constructor parameter is optional.
    assert!(!age.required);
}

#[test]
fn class_transform_ignores_unrelated_classes() {
    let mut request = analyze_request(vec![FieldSummary {
        name: "name".to_string(),
        annotation: Some(TypeExpr::annotation("str")),
        assigned_value: None,
        inferred_type: Some(TypeExpr::annotation("str")),
        has_default: false,
        source: SymbolSource::default(),
    }]);
    request.class.bases = vec![TypeExpr::expression("builtins.object")];

    assert_eq!(
        ModelClassTransformPlugin.analyze_class(&request),
        PluginResponse::NoChange
    );
}

#[test]
fn minidjango_synthesizes_model_fields_ids_and_manager() {
    let request = minidjango_analyze_request(vec![
        call_field(
            "title",
            "minidjango.CharField",
            vec![ArgumentSummary {
                name: Some("max_length".to_string()),
                kind: ArgumentKind::Keyword,
                type_expr: Some(TypeExpr::annotation("int")),
                value: LiteralValue::Int { value: 200 },
                source: None,
            }],
        ),
        call_field(
            "pages",
            "minidjango.IntegerField",
            vec![keyword_bool("null", true)],
        ),
        call_field(
            "author",
            "minidjango.ForeignKey",
            vec![class_arg("app.Author")],
        ),
        call_field("published", "app.BookManager", Vec::new()),
    ]);

    let PluginResponse::ClassPatch(patch) = MiniDjangoPlugin.analyze_class(&request) else {
        panic!("expected a class patch");
    };

    let title = patch
        .fields
        .iter()
        .find(|field| field.name == "title")
        .expect("title field");
    assert_eq!(title.instance_get_type.expression, "str");
    assert!(
        title
            .constructor_parameter
            .as_ref()
            .is_some_and(|parameter| parameter.required)
    );

    let pages = patch
        .fields
        .iter()
        .find(|field| field.name == "pages")
        .expect("pages field");
    assert_eq!(pages.instance_get_type.expression, "int | None");
    assert!(
        pages
            .constructor_parameter
            .as_ref()
            .is_some_and(|parameter| !parameter.required)
    );

    let author = patch
        .fields
        .iter()
        .find(|field| field.name == "author")
        .expect("author field");
    assert_eq!(author.instance_get_type.expression, "app.Author");

    let author_id = patch
        .fields
        .iter()
        .find(|field| field.name == "author_id")
        .expect("author_id field");
    assert_eq!(author_id.instance_get_type.expression, "int");
    assert!(author_id.constructor_parameter.is_none());

    assert!(
        patch
            .fields
            .iter()
            .any(|field| field.name == "id" && field.instance_get_type.expression == "int")
    );
    assert!(
        patch
            .fields
            .iter()
            .any(|field| field.name == "pk" && field.instance_get_type.expression == "int")
    );

    let manager_member_type = |name: &str| {
        patch
            .class_members
            .iter()
            .find(|member| member.name == name)
            .unwrap_or_else(|| panic!("expected `{name}` manager"))
            .access
            .instance_get_type()
            .expression
            .as_str()
    };
    assert_eq!(
        manager_member_type("objects"),
        "minidjango.virtual.app.Book.Manager"
    );
    assert_eq!(
        manager_member_type("_default_manager"),
        "minidjango.virtual.app.Book.Manager"
    );
    assert_eq!(
        manager_member_type("published"),
        "minidjango.virtual.app.Book.Manager"
    );
    assert!(
        patch.fields.iter().all(|field| field.name != "published"),
        "custom manager assignments should be class members, not instance fields"
    );
}

#[test]
fn minidjango_project_index_contributes_reverse_relation_members() {
    let author = ClassSummary {
        qualified_name: "app.Author".to_string(),
        bases: vec![TypeExpr::expression(minidjango::MODEL_BASE)],
        decorators: Vec::new(),
        metaclass: None,
        fields: Vec::new(),
        methods: Vec::new(),
        nested_classes: Vec::new(),
        class_constants: Vec::new(),
        source: SymbolSource::default(),
    };
    let book = minidjango_analyze_request(vec![call_field(
        "author",
        "minidjango.ForeignKey",
        vec![class_arg("app.Author")],
    )])
    .class;
    let request = BuildProjectIndexRequest {
        context: ProjectContext {
            root: "/project".to_string(),
            python_version: "3.13".to_string(),
            platform: "linux".to_string(),
            config: Value::default(),
        },
        classes: vec![author, book],
        settings: Vec::new(),
        assignments: Vec::new(),
        functions: Vec::new(),
        previous_index_fingerprint: None,
    };

    let PluginResponse::ProjectIndex(index) = MiniDjangoPlugin.build_project_index(&request) else {
        panic!("expected project index");
    };
    assert_eq!(
        index.plugin_index["models"]["app.Book"]["fields"]["author"],
        "app.Author"
    );
    assert_eq!(
        index.plugin_index["models"]["app.Book"]["fields"]["author_id"],
        "int"
    );
    let book_values_row = index
        .virtual_types
        .iter()
        .find(|definition| definition.name == "minidjango.virtual.app.Book.ValuesRow")
        .expect("Book values row virtual type");
    let VirtualTypeShape::TypedDict { fields, total } = &book_values_row.shape else {
        panic!("expected a typed-dict values row virtual type");
    };
    assert!(*total);
    assert!(
        fields
            .iter()
            .any(|field| field.name == "author" && field.type_expr.expression == "app.Author"),
        "virtual values row should preserve indexed model field types: {fields:#?}"
    );
    assert!(
        index.virtual_types.iter().any(|definition| definition.name
            == "minidjango.virtual.app.Book.ValuesListRow"
            && matches!(definition.shape, VirtualTypeShape::NamedTuple { .. })),
        "expected a reusable named-tuple row virtual type"
    );
    let book_manager = index
        .virtual_types
        .iter()
        .find(|definition| definition.name == "minidjango.virtual.app.Book.Manager")
        .expect("Book manager virtual type");
    let VirtualTypeShape::Class { bases, members } = &book_manager.shape else {
        panic!("expected a virtual manager class type");
    };
    assert_eq!(
        bases.first().map(|base| base.expression.as_str()),
        Some("minidjango.Manager[app.Book]")
    );
    assert!(members.is_empty());
    let [contribution] = index.contributions.as_slice() else {
        panic!("expected one reverse relation contribution");
    };
    assert!(matches!(
        &contribution.target,
        ContributionTarget::Instance { qualified_name } if qualified_name == "app.Author"
    ));
    let ContributionPatch::Field(field) = &contribution.patch else {
        panic!("expected field contribution");
    };
    assert_eq!(field.name, "book_set");
    assert_eq!(
        field.instance_get_type.expression,
        "minidjango.virtual.app.Book.Manager"
    );
    assert!(field.has_default);
}

#[test]
fn minidjango_project_index_reports_relation_target_and_reverse_conflicts() {
    let author = ClassSummary {
        qualified_name: "app.Author".to_string(),
        bases: vec![TypeExpr::expression(minidjango::MODEL_BASE)],
        decorators: Vec::new(),
        metaclass: None,
        fields: Vec::new(),
        methods: Vec::new(),
        nested_classes: Vec::new(),
        class_constants: Vec::new(),
        source: SymbolSource::default(),
    };
    let book = minidjango_analyze_request(vec![
        with_field_source(call_field(
            "author",
            "minidjango.ForeignKey",
            vec![class_arg("app.Author")],
        )),
        with_field_source(call_field(
            "editor",
            "minidjango.ForeignKey",
            vec![class_arg("app.Author")],
        )),
        call_field(
            "missing",
            "minidjango.ForeignKey",
            vec![positional_str("app.Missing")],
        ),
    ])
    .class;
    let request = BuildProjectIndexRequest {
        context: ProjectContext {
            root: "/project".to_string(),
            python_version: "3.13".to_string(),
            platform: "linux".to_string(),
            config: Value::default(),
        },
        classes: vec![author, book],
        settings: Vec::new(),
        assignments: Vec::new(),
        functions: Vec::new(),
        previous_index_fingerprint: None,
    };

    let PluginResponse::ProjectIndex(index) = MiniDjangoPlugin.build_project_index(&request) else {
        panic!("expected project index");
    };
    assert_eq!(
        index.contributions.len(),
        1,
        "the first reverse relation should win deterministically"
    );
    let messages = index
        .diagnostics
        .iter()
        .map(|diagnostic| diagnostic.message.as_str())
        .collect::<Vec<_>>();
    assert_eq!(
        messages,
        [
            "Conflicting Mini-Django reverse relation `app.Author.book_set`",
            "Unknown Mini-Django relation target `app.Missing` for field `app.Book.missing`",
        ]
    );
}

#[test]
fn minidjango_resolves_auth_user_model_from_settings_data() {
    let user = ClassSummary {
        qualified_name: "accounts.User".to_string(),
        bases: vec![TypeExpr::expression(minidjango::MODEL_BASE)],
        decorators: Vec::new(),
        metaclass: None,
        fields: Vec::new(),
        methods: Vec::new(),
        nested_classes: Vec::new(),
        class_constants: Vec::new(),
        source: SymbolSource::default(),
    };
    let book_request = minidjango_analyze_request(vec![call_field(
        "owner",
        "minidjango.ForeignKey",
        vec![
            setting_ref("minidjango_settings.AUTH_USER_MODEL"),
            keyword_str("related_name", "owned_books"),
        ],
    )]);
    let request = BuildProjectIndexRequest {
        context: ProjectContext {
            root: "/project".to_string(),
            python_version: "3.13".to_string(),
            platform: "linux".to_string(),
            config: Value::default(),
        },
        classes: vec![user, book_request.class.clone()],
        settings: vec![settings_module(
            "minidjango_settings",
            "AUTH_USER_MODEL",
            "accounts.User",
        )],
        assignments: Vec::new(),
        functions: Vec::new(),
        previous_index_fingerprint: None,
    };

    let PluginResponse::ProjectIndex(index) = MiniDjangoPlugin.build_project_index(&request) else {
        panic!("expected project index");
    };
    assert_eq!(
        index.plugin_index["settings"]["minidjango_settings.AUTH_USER_MODEL"],
        "accounts.User"
    );
    assert!(index.diagnostics.is_empty());
    let [contribution] = index.contributions.as_slice() else {
        panic!("expected auth user reverse relation contribution");
    };
    assert!(matches!(
        &contribution.target,
        ContributionTarget::Instance { qualified_name } if qualified_name == "accounts.User"
    ));
    let ContributionPatch::Field(field) = &contribution.patch else {
        panic!("expected field contribution");
    };
    assert_eq!(field.name, "owned_books");
    assert_eq!(
        field.instance_get_type.expression,
        "minidjango.virtual.app.Book.Manager"
    );

    let mut analyze_request = book_request;
    analyze_request.project_index = Some(index.plugin_index);
    let PluginResponse::ClassPatch(patch) = MiniDjangoPlugin.analyze_class(&analyze_request) else {
        panic!("expected class patch");
    };
    let owner = patch
        .fields
        .iter()
        .find(|field| field.name == "owner")
        .expect("owner field");
    assert_eq!(owner.instance_get_type.expression, "accounts.User");
}

#[test]
fn minidjango_resolves_string_and_self_foreign_key_targets() {
    let book_request = minidjango_analyze_request(vec![
        call_field(
            "author",
            "minidjango.ForeignKey",
            vec![
                positional_str("app.Author"),
                keyword_str("related_name", "books"),
            ],
        ),
        call_field(
            "parent",
            "minidjango.ForeignKey",
            vec![
                positional_str("self"),
                keyword_str("related_name", "children"),
                keyword_bool("null", true),
            ],
        ),
        call_field(
            "hidden",
            "minidjango.ForeignKey",
            vec![class_arg("app.Author"), keyword_str("related_name", "+")],
        ),
    ]);

    let PluginResponse::ClassPatch(patch) = MiniDjangoPlugin.analyze_class(&book_request) else {
        panic!("expected a class patch");
    };
    let field_type = |name: &str| {
        patch
            .fields
            .iter()
            .find(|field| field.name == name)
            .unwrap_or_else(|| panic!("expected field `{name}`"))
            .instance_get_type
            .expression
            .as_str()
    };
    assert_eq!(field_type("author"), "app.Author");
    assert_eq!(field_type("parent"), "app.Book | None");
    assert_eq!(field_type("hidden"), "app.Author");

    let author = ClassSummary {
        qualified_name: "app.Author".to_string(),
        bases: vec![TypeExpr::expression(minidjango::MODEL_BASE)],
        decorators: Vec::new(),
        metaclass: None,
        fields: Vec::new(),
        methods: Vec::new(),
        nested_classes: Vec::new(),
        class_constants: Vec::new(),
        source: SymbolSource::default(),
    };
    let request = BuildProjectIndexRequest {
        context: ProjectContext {
            root: "/project".to_string(),
            python_version: "3.13".to_string(),
            platform: "linux".to_string(),
            config: Value::default(),
        },
        classes: vec![author, book_request.class],
        settings: Vec::new(),
        assignments: Vec::new(),
        functions: Vec::new(),
        previous_index_fingerprint: None,
    };

    let PluginResponse::ProjectIndex(index) = MiniDjangoPlugin.build_project_index(&request) else {
        panic!("expected project index");
    };
    assert_eq!(index.contributions.len(), 2);

    let books = index
        .contributions
        .iter()
        .find(|contribution| {
            matches!(
                &contribution.target,
                ContributionTarget::Instance { qualified_name } if qualified_name == "app.Author"
            )
        })
        .expect("author reverse contribution");
    let ContributionPatch::Field(books_field) = &books.patch else {
        panic!("expected field contribution");
    };
    assert_eq!(books_field.name, "books");
    assert_eq!(
        books_field.instance_get_type.expression,
        "minidjango.virtual.app.Book.Manager"
    );

    let children = index
        .contributions
        .iter()
        .find(|contribution| {
            matches!(
                &contribution.target,
                ContributionTarget::Instance { qualified_name } if qualified_name == "app.Book"
            )
        })
        .expect("self reverse contribution");
    let ContributionPatch::Field(children_field) = &children.patch else {
        panic!("expected field contribution");
    };
    assert_eq!(children_field.name, "children");
    assert_eq!(
        children_field.instance_get_type.expression,
        "minidjango.virtual.app.Book.Manager"
    );
}

#[test]
fn minidjango_handles_defensive_request_shapes() {
    let plugin = MiniDjangoPlugin;

    let mut unrelated = minidjango_analyze_request(vec![]);
    unrelated.class.bases = vec![TypeExpr::expression("builtins.object")];
    assert_eq!(plugin.analyze_class(&unrelated), PluginResponse::NoChange);

    let PluginResponse::ClassPatch(patch) =
        plugin.analyze_class(&minidjango_analyze_request(vec![
            annotation_field("declared_only", Some(TypeExpr::annotation("str"))),
            call_field("unsupported", "minidjango.BooleanField", Vec::new()),
        ]))
    else {
        panic!("expected a class patch");
    };
    assert!(
        patch
            .fields
            .iter()
            .all(|field| field.name != "declared_only" && field.name != "unsupported")
    );

    let author = ClassSummary {
        qualified_name: "app.Author".to_string(),
        bases: vec![TypeExpr::expression(minidjango::MODEL_BASE)],
        decorators: Vec::new(),
        metaclass: None,
        fields: Vec::new(),
        methods: Vec::new(),
        nested_classes: Vec::new(),
        class_constants: Vec::new(),
        source: SymbolSource::default(),
    };
    let user = ClassSummary {
        qualified_name: "accounts.User".to_string(),
        bases: vec![TypeExpr::expression(minidjango::MODEL_BASE)],
        decorators: Vec::new(),
        metaclass: None,
        fields: Vec::new(),
        methods: Vec::new(),
        nested_classes: Vec::new(),
        class_constants: Vec::new(),
        source: SymbolSource::default(),
    };
    let unrelated_class = ClassSummary {
        qualified_name: "app.NotModel".to_string(),
        bases: vec![TypeExpr::expression("builtins.object")],
        decorators: Vec::new(),
        metaclass: None,
        fields: vec![call_field(
            "ignored",
            "minidjango.ForeignKey",
            vec![class_arg("app.Author")],
        )],
        methods: Vec::new(),
        nested_classes: Vec::new(),
        class_constants: Vec::new(),
        source: SymbolSource::default(),
    };
    let book = minidjango_analyze_request(vec![
        annotation_field("declared_only", Some(TypeExpr::annotation("str"))),
        call_field("manager", "app.BookManager", Vec::new()),
        call_field("unsupported", "minidjango.BooleanField", Vec::new()),
        call_field("no_target", "minidjango.ForeignKey", Vec::new()),
        with_field_source(call_field(
            "enum_author",
            "minidjango.ForeignKey",
            vec![
                ArgumentSummary {
                    name: None,
                    kind: ArgumentKind::Positional,
                    type_expr: Some(TypeExpr::annotation("app.Author")),
                    value: LiteralValue::EnumRef(SymbolRef {
                        qualified_name: "app.Author".to_string(),
                    }),
                    source: None,
                },
                keyword_str("related_name", "enum_books"),
            ],
        )),
        with_field_source(call_field(
            "local_author",
            "minidjango.ForeignKey",
            vec![
                positional_str("Author"),
                keyword_str("related_name", "local_books"),
            ],
        )),
        with_field_source(call_field(
            "settings_owner",
            "minidjango.ForeignKey",
            vec![
                positional_str("minidjango_settings.AUTH_USER_MODEL"),
                keyword_str("related_name", "settings_books"),
            ],
        )),
        with_field_source(call_field(
            "int_target",
            "minidjango.ForeignKey",
            vec![
                ArgumentSummary {
                    name: None,
                    kind: ArgumentKind::Positional,
                    type_expr: Some(TypeExpr::annotation("app.Author")),
                    value: LiteralValue::Int { value: 1 },
                    source: None,
                },
                keyword_str("related_name", "int_books"),
            ],
        )),
        with_field_source(call_field(
            "bad_related_name",
            "minidjango.ForeignKey",
            vec![class_arg("app.Author"), keyword_bool("related_name", true)],
        )),
    ])
    .class;
    let request = BuildProjectIndexRequest {
        context: ProjectContext {
            root: "/project".to_string(),
            python_version: "3.13".to_string(),
            platform: "linux".to_string(),
            config: Value::default(),
        },
        classes: vec![unrelated_class, author, user, book],
        settings: vec![mixed_settings_module()],
        assignments: Vec::new(),
        functions: Vec::new(),
        previous_index_fingerprint: None,
    };
    let PluginResponse::ProjectIndex(index) = plugin.build_project_index(&request) else {
        panic!("expected project index");
    };
    assert!(index.plugin_index["models"].get("app.NotModel").is_none());
    assert!(
        index.plugin_index["models"]["app.Book"]["fields"]
            .get("manager")
            .is_none()
    );
    assert!(
        index.plugin_index["models"]["app.Book"]["fields"]
            .get("unsupported")
            .is_none()
    );
    assert_eq!(
        index.plugin_index["models"]["app.Book"]["fields"]["local_author"],
        "app.Author"
    );
    assert_eq!(
        index.plugin_index["models"]["app.Book"]["fields"]["settings_owner"],
        "accounts.User"
    );
    assert!(
        index
            .contributions
            .iter()
            .any(|contribution| contribution.conflict_key == "app.Author.int_books")
    );

    let receiver = |base: &str, generic_arguments: Vec<TypeExpr>| ReceiverSummary {
        type_expr: TypeExpr::annotation(base),
        nominal_class: Some(base.to_string()),
        generic_arguments,
        plugin_metadata: Value::default(),
    };
    let call_request = |method: &str,
                        receiver: Option<ReceiverSummary>,
                        arguments: Vec<ArgumentSummary>,
                        project_index: Option<_>| CallRequest {
        context: context(),
        callee: TypeExpr::expression(format!("minidjango.Manager.{method}")),
        receiver,
        arguments,
        existing_signature: None,
        default_return_type: None,
        project_index,
    };
    let indexed = json!({
        "models": {
            "app.Book": {
                "fields": {
                    "id": "int",
                    "pk": "int",
                    "title": "str",
                    "pages": "int | None",
                    "author": "app.Author",
                    "author_id": "int"
                }
            },
            "app.Author": {
                "fields": {
                    "id": "int",
                    "pk": "int",
                    "name": "str"
                }
            }
        }
    });

    assert_eq!(
        plugin.adjust_call_return(&call_request("filter", None, Vec::new(), None)),
        PluginResponse::NoChange
    );
    assert_eq!(
        plugin.adjust_call_return(&call_request(
            "filter",
            Some(receiver(
                "other.Manager",
                vec![TypeExpr::annotation("app.Book")]
            )),
            Vec::new(),
            None
        )),
        PluginResponse::NoChange
    );
    assert_eq!(
        plugin.adjust_call_return(&call_request(
            "filter",
            Some(receiver(minidjango::MANAGER_BASE, Vec::new())),
            Vec::new(),
            None
        )),
        PluginResponse::NoChange
    );

    let return_type = |response| {
        let PluginResponse::CallReturnPatch(patch) = response else {
            panic!("expected call return patch");
        };
        patch.return_type.expression
    };
    let manager = Some(receiver(
        minidjango::MANAGER_BASE,
        vec![TypeExpr::annotation("app.Book")],
    ));
    assert_eq!(
        return_type(plugin.adjust_call_return(&call_request(
            "get",
            manager.clone(),
            Vec::new(),
            Some(indexed.clone())
        ))),
        "app.Book"
    );
    assert_eq!(
        return_type(plugin.adjust_call_return(&call_request(
            "first",
            manager.clone(),
            Vec::new(),
            Some(indexed.clone())
        ))),
        "app.Book | None"
    );
    assert_eq!(
        plugin.adjust_call_return(&call_request(
            "unknown",
            manager.clone(),
            Vec::new(),
            Some(indexed.clone())
        )),
        PluginResponse::NoChange
    );
    assert!(
        return_type(plugin.adjust_call_return(&call_request(
            "values_list",
            manager.clone(),
            Vec::new(),
            Some(indexed.clone())
        )))
        .starts_with("minidjango.QuerySet[app.Book, tuple[")
    );
    assert_eq!(
        plugin.adjust_call_return(&call_request(
            "values_list",
            manager.clone(),
            vec![keyword_bool("flat", true)],
            Some(indexed.clone())
        )),
        PluginResponse::NoChange
    );
    assert!(
        return_type(plugin.adjust_call_return(&call_request(
            "values_list",
            manager.clone(),
            vec![positional_int(1)],
            Some(indexed.clone())
        )))
        .contains("tuple[")
    );
    assert_eq!(
        return_type(plugin.adjust_call_return(&call_request(
            "values_list",
            manager.clone(),
            vec![positional_str("title"), keyword_str("flat", "yes")],
            Some(indexed.clone())
        ))),
        "minidjango.QuerySet[app.Book, tuple[str]]"
    );
    assert_eq!(
        return_type(plugin.adjust_call_return(&call_request(
            "annotate",
            manager.clone(),
            vec![keyword_int("score", 1)],
            Some(indexed.clone())
        ))),
        r#"minidjango.QuerySet[app.Book, Class("MiniDjangoAnnotatedRow", {"score": int}, app.Book)]"#
    );
    let PluginResponse::CallReturnPatch(patch) = plugin.adjust_call_return(&call_request(
        "filter",
        manager.clone(),
        vec![
            keyword_str("title__iexact", "Ada"),
            keyword_str("title__regex", "^A"),
            keyword_str("title__iregex", "^a"),
            keyword_tuple(
                "pages__range",
                vec![
                    LiteralValue::Int { value: 1 },
                    LiteralValue::Int { value: 10 },
                ],
            ),
        ],
        Some(indexed.clone()),
    )) else {
        panic!("expected call return patch");
    };
    assert!(patch.diagnostics.is_empty());
    assert_eq!(
        patch.return_type.expression,
        "minidjango.QuerySet[app.Book, app.Book]"
    );

    let PluginResponse::CallReturnPatch(patch) = plugin.adjust_call_return(&call_request(
        "filter",
        manager,
        vec![
            with_arg_source(keyword_str("exact", "bad")),
            with_arg_source(keyword_str("missing__name", "bad")),
            with_arg_source(keyword_str("", "bad")),
        ],
        Some(indexed),
    )) else {
        panic!("expected call return patch");
    };
    assert_eq!(patch.diagnostics.len(), 3);
    assert!(
        patch
            .diagnostics
            .iter()
            .all(|diagnostic| diagnostic.location.is_some())
    );
}

#[test]
fn call_return_overrides_field_return_type() {
    let request = CallRequest {
        context: context(),
        callee: TypeExpr::expression(call_return::FIELD_FUNCTION),
        receiver: None,
        arguments: Vec::new(),
        existing_signature: None,
        default_return_type: None,
        project_index: None,
    };

    let PluginResponse::CallReturnPatch(patch) = FieldCallReturnPlugin.adjust_call_return(&request)
    else {
        panic!("expected a call-return patch");
    };
    assert_eq!(patch.return_type.expression, "str");
}

#[test]
fn minidjango_manager_return_hooks_use_receiver_and_arguments() {
    let receiver = |base: &str| {
        let generic_arguments = if base == minidjango::QUERYSET_BASE {
            vec![
                TypeExpr::annotation("app.Book"),
                TypeExpr::annotation("app.Book"),
            ]
        } else {
            vec![TypeExpr::annotation("app.Book")]
        };
        let type_expr = if base == minidjango::QUERYSET_BASE {
            TypeExpr::annotation(format!("{base}[app.Book, app.Book]"))
        } else {
            TypeExpr::annotation(format!("{base}[app.Book]"))
        };
        ReceiverSummary {
            type_expr,
            nominal_class: Some(base.to_string()),
            generic_arguments,
            plugin_metadata: Value::default(),
        }
    };
    let request = |base: &str, method: &str, arguments: Vec<ArgumentSummary>| CallRequest {
        context: context(),
        callee: TypeExpr::expression(format!("{base}.{method}")),
        receiver: Some(receiver(base)),
        arguments,
        existing_signature: None,
        default_return_type: None,
        project_index: None,
    };
    let indexed_request = |base: &str, method: &str, arguments: Vec<ArgumentSummary>| {
        let mut request = request(base, method, arguments);
        request.project_index = Some(json!({
            "models": {
                "app.Book": {
                    "fields": {
                        "id": "int",
                        "pk": "int",
                        "title": "str",
                        "pages": "int | None",
                        "active": "bool",
                        "author": "app.Author",
                        "author_id": "int"
                    }
                },
                "app.Author": {
                    "fields": {
                        "id": "int",
                        "pk": "int",
                        "name": "str"
                    }
                }
            }
        }));
        request
    };
    let return_patch = |response| {
        let PluginResponse::CallReturnPatch(patch) = response else {
            panic!("expected a call-return patch");
        };
        patch
    };
    let return_type = |response| return_patch(response).return_type.expression;

    assert_eq!(
        return_type(MiniDjangoPlugin.adjust_call_return(&request(
            minidjango::MANAGER_BASE,
            "filter",
            vec![keyword_str("title", "ok")]
        ))),
        "minidjango.QuerySet[app.Book, app.Book]"
    );
    assert_eq!(
        return_type(MiniDjangoPlugin.adjust_call_return(&request(
            minidjango::QUERYSET_BASE,
            "filter",
            Vec::new()
        ))),
        "minidjango.QuerySet[app.Book, app.Book]"
    );
    assert_eq!(
        return_type(MiniDjangoPlugin.adjust_call_return(&request(
            minidjango::QUERYSET_BASE,
            "get",
            vec![keyword_str("title", "ok")]
        ))),
        "app.Book"
    );
    assert_eq!(
        return_type(MiniDjangoPlugin.adjust_call_return(&request(
            minidjango::MANAGER_BASE,
            "get_or_create",
            vec![keyword_str("title", "ok")]
        ))),
        "tuple[app.Book, bool]"
    );
    assert_eq!(
        return_type(MiniDjangoPlugin.adjust_call_return(&request(
            minidjango::QUERYSET_BASE,
            "get_or_create",
            vec![keyword_str("title", "ok")]
        ))),
        "tuple[app.Book, bool]"
    );
    assert_eq!(
        return_type(MiniDjangoPlugin.adjust_call_return(&request(
            minidjango::QUERYSET_BASE,
            "first",
            Vec::new()
        ))),
        "app.Book | None"
    );
    assert_eq!(
        return_type(MiniDjangoPlugin.adjust_call_return(&request(
            minidjango::MANAGER_BASE,
            "values",
            vec![positional_str("title"), positional_str("pages")]
        ))),
        "minidjango.QuerySet[app.Book, dict[str, object]]"
    );
    assert_eq!(
        return_type(MiniDjangoPlugin.adjust_call_return(&request(
            minidjango::QUERYSET_BASE,
            "values",
            vec![positional_str("title")]
        ))),
        "minidjango.QuerySet[app.Book, dict[str, object]]"
    );
    assert_eq!(
        return_type(MiniDjangoPlugin.adjust_call_return(&indexed_request(
            minidjango::MANAGER_BASE,
            "values",
            Vec::new()
        ))),
        "minidjango.QuerySet[app.Book, minidjango.virtual.app.Book.ValuesRow]"
    );
    assert_eq!(
        return_type(MiniDjangoPlugin.adjust_call_return(&indexed_request(
            minidjango::MANAGER_BASE,
            "values",
            vec![positional_str("title"), positional_str("pages")]
        ))),
        r#"minidjango.QuerySet[app.Book, TypedDict({"title": str, "pages": int | None})]"#
    );
    assert_eq!(
        return_type(MiniDjangoPlugin.adjust_call_return(&request(
            minidjango::MANAGER_BASE,
            "values_list",
            vec![positional_str("title"), keyword_bool("flat", true)]
        ))),
        "minidjango.QuerySet[app.Book, str]"
    );
    assert_eq!(
        return_type(MiniDjangoPlugin.adjust_call_return(&request(
            minidjango::QUERYSET_BASE,
            "values_list",
            vec![positional_str("title"), keyword_bool("flat", true)]
        ))),
        "minidjango.QuerySet[app.Book, str]"
    );
    assert_eq!(
        return_type(MiniDjangoPlugin.adjust_call_return(&indexed_request(
            minidjango::MANAGER_BASE,
            "values_list",
            vec![positional_str("pages"), keyword_bool("flat", true)]
        ))),
        "minidjango.QuerySet[app.Book, int | None]"
    );
    assert_eq!(
        return_type(MiniDjangoPlugin.adjust_call_return(&indexed_request(
            minidjango::MANAGER_BASE,
            "values_list",
            vec![positional_str("title"), positional_str("pages")]
        ))),
        "minidjango.QuerySet[app.Book, tuple[str, int | None]]"
    );
    assert_eq!(
        return_type(MiniDjangoPlugin.adjust_call_return(&indexed_request(
            minidjango::MANAGER_BASE,
            "values_list",
            vec![keyword_bool("named", true)]
        ))),
        "minidjango.QuerySet[app.Book, minidjango.virtual.app.Book.ValuesListRow]"
    );
    assert_eq!(
        return_type(MiniDjangoPlugin.adjust_call_return(&indexed_request(
            minidjango::MANAGER_BASE,
            "values_list",
            vec![
                positional_str("title"),
                positional_str("pages"),
                keyword_bool("named", true)
            ]
        ))),
        r#"minidjango.QuerySet[app.Book, NamedTuple("MiniDjangoValuesListRow", {"title": str, "pages": int | None})]"#
    );
    assert_eq!(
        return_type(MiniDjangoPlugin.adjust_call_return(&indexed_request(
            minidjango::QUERYSET_BASE,
            "values_list",
            vec![positional_str("author"), positional_str("author_id")]
        ))),
        "minidjango.QuerySet[app.Book, tuple[app.Author, int]]"
    );
    assert_eq!(
        return_type(MiniDjangoPlugin.adjust_call_return(&indexed_request(
            minidjango::MANAGER_BASE,
            "annotate",
            Vec::new()
        ))),
        "minidjango.QuerySet[app.Book, app.Book]"
    );
    assert_eq!(
        return_type(MiniDjangoPlugin.adjust_call_return(&indexed_request(
            minidjango::MANAGER_BASE,
            "annotate",
            vec![
                keyword_value("flag", LiteralValue::Bool { value: true }),
                keyword_value("score", LiteralValue::Int { value: 1 }),
                keyword_value(
                    "label",
                    LiteralValue::Str {
                        value: "ok".to_string(),
                    },
                ),
                keyword_value("missing", LiteralValue::None),
                keyword_value(
                    "payload",
                    LiteralValue::Dict {
                        entries: Vec::new(),
                    },
                ),
            ]
        ))),
        r#"minidjango.QuerySet[app.Book, Class("MiniDjangoAnnotatedRow", {"flag": bool, "score": int, "label": str, "missing": None, "payload": object}, app.Book)]"#
    );
    let patch = return_patch(MiniDjangoPlugin.adjust_call_return(&indexed_request(
        minidjango::MANAGER_BASE,
        "filter",
        vec![
            keyword_str("title__exact", "ok"),
            keyword_str("missing", "bad"),
        ],
    )));
    assert_eq!(
        patch.return_type.expression,
        "minidjango.QuerySet[app.Book, app.Book]"
    );
    let [diagnostic] = patch.diagnostics.as_slice() else {
        panic!("expected one invalid lookup diagnostic");
    };
    assert_eq!(diagnostic.id, "minidjango.unknown-lookup");
    assert_eq!(
        diagnostic.message,
        "Unknown Mini-Django lookup `missing` for model `app.Book`"
    );

    let patch = return_patch(MiniDjangoPlugin.adjust_call_return(&indexed_request(
        minidjango::MANAGER_BASE,
        "filter",
        vec![
            keyword_value("title", LiteralValue::Unknown),
            keyword_value("pages", LiteralValue::None),
            keyword_value("active", LiteralValue::Bool { value: true }),
            keyword_value(
                "author",
                LiteralValue::ClassRef(SymbolRef {
                    qualified_name: "app.Author".to_string(),
                }),
            ),
            keyword_str("title__contains", "ok"),
            keyword_value("title__contains", LiteralValue::Unknown),
            keyword_bool("pages__isnull", true),
            keyword_tuple(
                "pages__range",
                vec![
                    LiteralValue::Int { value: 1 },
                    LiteralValue::Int { value: 10 },
                ],
            ),
            keyword_value("pages__range", LiteralValue::Unknown),
            keyword_list(
                "title__in",
                vec![
                    LiteralValue::Str {
                        value: "A".to_string(),
                    },
                    LiteralValue::Str {
                        value: "B".to_string(),
                    },
                ],
            ),
            keyword_value("title__in", LiteralValue::Unknown),
            keyword_str("author__name", "Ada"),
            keyword_str("author__name__icontains", "Ada"),
        ],
    )));
    assert_eq!(
        patch.return_type.expression,
        "minidjango.QuerySet[app.Book, app.Book]"
    );
    assert!(
        patch.diagnostics.is_empty(),
        "supported terminal and relation lookups should not produce diagnostics: {:#?}",
        patch.diagnostics
    );

    let patch = return_patch(MiniDjangoPlugin.adjust_call_return(&indexed_request(
        minidjango::MANAGER_BASE,
        "filter",
        vec![
            keyword_list("title", Vec::new()),
            keyword_str("pages__isnull", "yes"),
            keyword_str("pages__gt", "many"),
            keyword_int("title__contains", 1),
            keyword_tuple(
                "pages__range",
                vec![
                    LiteralValue::Int { value: 1 },
                    LiteralValue::Str {
                        value: "many".to_string(),
                    },
                ],
            ),
            keyword_tuple("pages__range", vec![LiteralValue::Int { value: 1 }]),
            keyword_list(
                "title__in",
                vec![
                    LiteralValue::Str {
                        value: "A".to_string(),
                    },
                    LiteralValue::Int { value: 1 },
                ],
            ),
            keyword_str("title__in", "A"),
        ],
    )));
    assert_eq!(
        patch.return_type.expression,
        "minidjango.QuerySet[app.Book, app.Book]"
    );
    assert_eq!(patch.diagnostics.len(), 8);
    assert!(
        patch
            .diagnostics
            .iter()
            .all(|diagnostic| diagnostic.id == "minidjango.invalid-lookup-value")
    );
    assert!(patch.diagnostics.iter().any(|diagnostic| diagnostic.message
        == "Invalid Mini-Django lookup value for `pages__isnull` on `app.Book.pages`; expected `int | None`"));
    assert!(patch.diagnostics.iter().any(|diagnostic| diagnostic.message
        == "Invalid Mini-Django lookup value for `title__contains` on `app.Book.title`; expected `str`"));

    let patch = return_patch(MiniDjangoPlugin.adjust_call_return(&indexed_request(
        minidjango::MANAGER_BASE,
        "filter",
        vec![
            keyword_str("title__year", "bad"),
            keyword_str("author__missing", "bad"),
        ],
    )));
    assert_eq!(
        patch.return_type.expression,
        "minidjango.QuerySet[app.Book, app.Book]"
    );
    assert_eq!(patch.diagnostics.len(), 2);
    assert!(patch.diagnostics.iter().any(|diagnostic| diagnostic.message
        == "Unknown Mini-Django lookup `title__year` for model `app.Book`"));
    assert!(patch.diagnostics.iter().any(|diagnostic| diagnostic.message
        == "Unknown Mini-Django lookup `author__missing` for model `app.Book`"));

    let patch = return_patch(MiniDjangoPlugin.adjust_call_return(&indexed_request(
        minidjango::MANAGER_BASE,
        "values",
        vec![positional_str("missing")],
    )));
    assert_eq!(
        patch.return_type.expression,
        r#"minidjango.QuerySet[app.Book, TypedDict({"missing": object})]"#
    );
    let [diagnostic] = patch.diagnostics.as_slice() else {
        panic!("expected one invalid values field diagnostic");
    };
    assert_eq!(
        diagnostic.message,
        "Unknown Mini-Django lookup `missing` for model `app.Book`"
    );

    let patch = return_patch(MiniDjangoPlugin.adjust_call_return(&indexed_request(
        minidjango::QUERYSET_BASE,
        "values_list",
        vec![positional_str("missing"), keyword_bool("flat", true)],
    )));
    assert_eq!(
        patch.return_type.expression,
        "minidjango.QuerySet[app.Book, str]"
    );
    let [diagnostic] = patch.diagnostics.as_slice() else {
        panic!("expected one invalid values_list field diagnostic");
    };
    assert_eq!(
        diagnostic.message,
        "Unknown Mini-Django lookup `missing` for model `app.Book`"
    );
    assert!(matches!(
        MiniDjangoPlugin.adjust_call_return(&request(
            minidjango::QUERYSET_BASE,
            "values_list",
            vec![positional_str("title"), keyword_bool("flat", false)]
        )),
        PluginResponse::NoChange
    ));
    assert_eq!(
        return_type(MiniDjangoPlugin.adjust_call_return(&request(
            minidjango::QUERYSET_BASE,
            "count",
            Vec::new()
        ))),
        "int"
    );
    assert_eq!(
        return_type(MiniDjangoPlugin.adjust_call_return(&request(
            minidjango::QUERYSET_BASE,
            "exists",
            Vec::new()
        ))),
        "bool"
    );
}

#[test]
fn every_example_manifest_enables_its_claimed_capabilities() {
    let plugins: [&dyn Plugin; 4] = [
        &StubOverlayPlugin,
        &ModelClassTransformPlugin,
        &FieldCallReturnPlugin,
        &MiniDjangoPlugin,
    ];

    for plugin in plugins {
        let manifest = plugin.manifest();
        let capabilities = &manifest.capabilities;

        if !manifest.claims.classes.is_empty() {
            assert!(
                capabilities.class_transform,
                "{} class-transform",
                manifest.id
            );
        }
        if !manifest.claims.functions.is_empty() {
            assert!(
                capabilities.call_signature || capabilities.call_return,
                "{} call hook",
                manifest.id
            );
        }
        if !manifest.stub_overlays.is_empty() {
            assert!(capabilities.stub_overlays, "{} stub-overlays", manifest.id);
        }
    }
}

#[test]
fn minidjango_manifest_uses_only_sdk_protocol_claims() {
    let manifest = MiniDjangoPlugin.manifest();

    assert!(manifest.capabilities.class_transform);
    assert!(manifest.capabilities.project_index);
    assert!(manifest.capabilities.cross_symbol_contributions);
    assert!(manifest.capabilities.call_return);
    assert!(manifest.capabilities.settings_data);
    assert!(manifest.capabilities.virtual_types);

    assert!(manifest.claims.classes.iter().any(|claim| matches!(
        &claim.kind,
        ClassClaimKind::SubclassOf {
            base_qualified_name
        } if base_qualified_name == minidjango::MODEL_BASE
    )));
    assert!(manifest.claims.methods.iter().any(|claim| matches!(
        &claim.kind,
        MethodClaimKind::OnSubclassOf {
            base_qualified_name,
            method_name
        } if base_qualified_name == minidjango::MANAGER_BASE && method_name == "filter"
    )));
}
