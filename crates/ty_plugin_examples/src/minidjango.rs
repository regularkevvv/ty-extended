//! Example: a Mini-Django plugin.
//!
//! This module intentionally depends only on `ty_plugin_sdk`. It is the permanent SDK-only proof
//! that a Django-shaped plugin can declare and implement behavior without importing checker
//! internals.

use std::collections::{BTreeMap, BTreeSet};

use ty_plugin_sdk::dsl::{self, ClassPatchBuilder};
use ty_plugin_sdk::protocol::{
    AnalyzeClassRequest, ArgumentKind, ArgumentSummary, AssignedValueSummary,
    BuildProjectIndexRequest, CallRequest, CallReturnPatch, CallValueSummary, Contribution,
    ContributionPatch, ContributionTarget, DiagnosticLocation, DiagnosticSeverity, FieldPatch,
    LiteralValue, MemberAccessPatch, MemberPatchMode, PluginDiagnostic, PluginManifest,
    PluginResponse, ProjectIndexResponse, SymbolSource, TypeExpr, VirtualTypeDefinition,
    VirtualTypeField, VirtualTypeShape,
};
use ty_plugin_sdk::serde_json::{self, Value, json};
use ty_plugin_sdk::{ManifestBuilder, Plugin};

/// The model base class whose subclasses the Mini-Django plugin indexes and transforms.
pub const MODEL_BASE: &str = "minidjango.Model";
/// The generic manager base class used for receiver-aware queryset methods.
pub const MANAGER_BASE: &str = "minidjango.Manager";
/// The generic queryset base class used for receiver-aware chained queryset methods.
pub const QUERYSET_BASE: &str = "minidjango.QuerySet";

#[derive(Debug, Default, Clone, Copy)]
pub struct MiniDjangoPlugin;

impl Plugin for MiniDjangoPlugin {
    fn manifest(&self) -> PluginManifest {
        ManifestBuilder::new("example.minidjango", "Mini-Django proof plugin", "0.1.0")
            .claim_subclass_transform(MODEL_BASE)
            .claim_instance_contribution_target(MODEL_BASE)
            .claim_call_return_method_on_subclass(MANAGER_BASE, "filter")
            .claim_call_return_method_on_subclass(MANAGER_BASE, "get")
            .claim_call_return_method_on_subclass(MANAGER_BASE, "get_or_create")
            .claim_call_return_method_on_subclass(MANAGER_BASE, "first")
            .claim_call_return_method_on_subclass(MANAGER_BASE, "count")
            .claim_call_return_method_on_subclass(MANAGER_BASE, "exists")
            .claim_call_return_method_on_subclass(MANAGER_BASE, "values")
            .claim_call_return_method_on_subclass(MANAGER_BASE, "values_list")
            .claim_call_return_method_on_subclass(MANAGER_BASE, "annotate")
            .claim_call_return_method_on_subclass(QUERYSET_BASE, "filter")
            .claim_call_return_method_on_subclass(QUERYSET_BASE, "get")
            .claim_call_return_method_on_subclass(QUERYSET_BASE, "get_or_create")
            .claim_call_return_method_on_subclass(QUERYSET_BASE, "first")
            .claim_call_return_method_on_subclass(QUERYSET_BASE, "count")
            .claim_call_return_method_on_subclass(QUERYSET_BASE, "exists")
            .claim_call_return_method_on_subclass(QUERYSET_BASE, "values")
            .claim_call_return_method_on_subclass(QUERYSET_BASE, "values_list")
            .claim_call_return_method_on_subclass(QUERYSET_BASE, "annotate")
            .settings_module("minidjango_settings")
            .virtual_types()
            .build()
    }

    fn build_project_index(&self, request: &BuildProjectIndexRequest) -> PluginResponse {
        let mut contributions = Vec::new();
        let mut diagnostics = Vec::new();
        let mut models = serde_json::Map::new();
        let settings_values = settings_value_index(request);
        let model_names = request
            .classes
            .iter()
            .filter(|class| derives_from_model_class(class))
            .map(|class| class.qualified_name.clone())
            .collect::<BTreeSet<_>>();
        let mut reverse_names = BTreeMap::new();
        let mut virtual_types = Vec::new();

        for class in &request.classes {
            if !derives_from_model_class(class) {
                continue;
            }

            let fields = model_field_index(class, &settings_values);
            virtual_types.extend(model_virtual_type_definitions(
                &class.qualified_name,
                &fields,
            ));
            models.insert(
                class.qualified_name.clone(),
                json!({
                    "fields": fields,
                }),
            );

            for field in &class.fields {
                let Some(call) = field_call(field.assigned_value.as_ref()) else {
                    continue;
                };
                if !is_foreign_key_call(call) {
                    continue;
                }

                let Some(target) = relation_target_type(
                    class_module_name(&class.qualified_name),
                    &class.qualified_name,
                    call,
                    &settings_values,
                ) else {
                    continue;
                };
                if !model_names.contains(&target.expression) {
                    diagnostics.push(unknown_relation_target_diagnostic(
                        &class.qualified_name,
                        &field.name,
                        &target.expression,
                        &field.source,
                    ));
                    continue;
                }
                let Some(reverse_name) = reverse_relation_name(&class.qualified_name, call) else {
                    continue;
                };
                let conflict_key = format!("{}.{}", target.expression, reverse_name);
                if let Some(first_source) =
                    reverse_names.insert(conflict_key.clone(), field.source.clone())
                {
                    diagnostics.push(reverse_relation_conflict_diagnostic(
                        &target.expression,
                        &reverse_name,
                        &field.source,
                        &first_source,
                    ));
                    continue;
                }
                contributions.push(Contribution {
                    source: field.source.clone(),
                    target: ContributionTarget::Instance {
                        qualified_name: target.expression.clone(),
                    },
                    patch: ContributionPatch::Field(FieldPatch {
                        mode: MemberPatchMode::FillOnMiss,
                        name: reverse_name.clone(),
                        descriptor: None,
                        instance_get_type: TypeExpr::annotation(model_manager_virtual_type_name(
                            &class.qualified_name,
                        )),
                        instance_set_type: None,
                        constructor_parameter: None,
                        has_default: true,
                    }),
                    conflict_key,
                    diagnostics: Vec::new(),
                });
            }
        }

        PluginResponse::ProjectIndex(ProjectIndexResponse {
            class_indexes: BTreeMap::new(),
            plugin_index: json!({
                "models": models,
                "settings": settings_values,
            }),
            contributions,
            virtual_types,
            dependencies: Vec::new(),
            diagnostics,
        })
    }

    fn analyze_class(&self, request: &AnalyzeClassRequest) -> PluginResponse {
        if !derives_from_model(request) {
            return PluginResponse::NoChange;
        }

        let manager_type = TypeExpr::annotation(model_manager_virtual_type_name(
            &request.class.qualified_name,
        ));
        let settings_values =
            settings_value_index_from_project_index(request.project_index.as_ref());
        let mut patch = ClassPatchBuilder::new()
            .field(non_init_field("id", TypeExpr::annotation("int")))
            .field(non_init_field("pk", TypeExpr::annotation("int")))
            .class_member(dsl::member("objects", manager_type.clone()))
            .class_member(dsl::member("_default_manager", manager_type.clone()));

        for field in &request.class.fields {
            let Some(call) = field_call(field.assigned_value.as_ref()) else {
                continue;
            };

            if is_manager_call(call) {
                patch = patch.class_member(dsl::member(&field.name, manager_type.clone()));
                continue;
            }

            let Some(mut field_patch) =
                field_patch_from_call(request, &field.name, call, &settings_values)
            else {
                continue;
            };
            field_patch.has_default = field.has_default || field_has_null_true(call);
            if field_patch.has_default
                && let Some(parameter) = field_patch.constructor_parameter.as_mut()
            {
                parameter.required = false;
            }

            if is_foreign_key_call(call) {
                patch = patch.field(foreign_key_id_field(&field.name, field_has_null_true(call)));
            }

            patch = patch.field(field_patch);
        }

        patch.response()
    }

    fn adjust_call_return(&self, request: &CallRequest) -> PluginResponse {
        let Some(receiver) = &request.receiver else {
            return PluginResponse::NoChange;
        };
        if !matches!(
            receiver.nominal_class.as_deref(),
            Some(MANAGER_BASE | QUERYSET_BASE)
        ) {
            return PluginResponse::NoChange;
        }

        let method_name = request
            .callee
            .expression
            .rsplit('.')
            .next()
            .unwrap_or_default();
        let receiver_is_queryset = receiver.nominal_class.as_deref() == Some(QUERYSET_BASE);
        let Some(model_type) = receiver.generic_arguments.first().cloned() else {
            return PluginResponse::NoChange;
        };
        let row_type = if receiver_is_queryset {
            receiver
                .generic_arguments
                .get(1)
                .cloned()
                .unwrap_or_else(|| model_type.clone())
        } else {
            model_type.clone()
        };
        let diagnostics = validate_lookup_arguments(method_name, &model_type.expression, request);

        match method_name {
            "filter" => call_return(queryset_type(&model_type, &row_type), diagnostics),
            "get" => call_return(
                if receiver_is_queryset {
                    row_type
                } else {
                    model_type
                },
                diagnostics,
            ),
            "get_or_create" => call_return(
                TypeExpr::annotation(format!("tuple[{}, bool]", model_type.expression)),
                diagnostics,
            ),
            "first" => call_return(
                TypeExpr::annotation(format!(
                    "{} | None",
                    if receiver_is_queryset {
                        row_type.expression
                    } else {
                        model_type.expression
                    }
                )),
                diagnostics,
            ),
            "count" => call_return(TypeExpr::annotation("int"), diagnostics),
            "exists" => call_return(TypeExpr::annotation("bool"), diagnostics),
            "values" => call_return(
                queryset_type(
                    &model_type,
                    &values_row_type(request, &model_type.expression)
                        .unwrap_or_else(|| TypeExpr::annotation("dict[str, object]")),
                ),
                diagnostics,
            ),
            "values_list" => {
                let Some(row_type) = values_list_row_type(request, &model_type.expression) else {
                    return PluginResponse::NoChange;
                };
                call_return(queryset_type(&model_type, &row_type), diagnostics)
            }
            "annotate" => {
                let row_type = annotated_row_type(request, &row_type).unwrap_or(row_type);
                call_return(queryset_type(&model_type, &row_type), diagnostics)
            }
            _ => PluginResponse::NoChange,
        }
    }
}

fn call_return(return_type: TypeExpr, diagnostics: Vec<PluginDiagnostic>) -> PluginResponse {
    PluginResponse::CallReturnPatch(CallReturnPatch {
        return_type,
        diagnostics,
        result_metadata: None,
    })
}

fn queryset_type(model_type: &TypeExpr, row_type: &TypeExpr) -> TypeExpr {
    TypeExpr::annotation(format!(
        "{QUERYSET_BASE}[{}, {}]",
        model_type.expression, row_type.expression
    ))
}

fn annotated_row_type(request: &CallRequest, base_row_type: &TypeExpr) -> Option<TypeExpr> {
    let entries = request
        .arguments
        .iter()
        .filter(|argument| argument.kind == ArgumentKind::Keyword)
        .filter_map(|argument| {
            let name = argument.name.as_deref()?;
            let key = serde_json::to_string(name).ok()?;
            Some(format!(
                "{key}: {}",
                annotation_argument_type(argument).expression
            ))
        })
        .collect::<Vec<_>>();
    if entries.is_empty() {
        return None;
    }

    Some(TypeExpr::annotation(format!(
        r#"Class("MiniDjangoAnnotatedRow", {{{}}}, {})"#,
        entries.join(", "),
        base_row_type.expression
    )))
}

fn annotation_argument_type(argument: &ArgumentSummary) -> TypeExpr {
    argument
        .type_expr
        .clone()
        .unwrap_or_else(|| match &argument.value {
            LiteralValue::Bool { .. } => TypeExpr::annotation("bool"),
            LiteralValue::Int { .. } => TypeExpr::annotation("int"),
            LiteralValue::Str { .. } => TypeExpr::annotation("str"),
            LiteralValue::None => TypeExpr::annotation("None"),
            _ => TypeExpr::annotation("object"),
        })
}

fn values_list_row_type(request: &CallRequest, model_name: &str) -> Option<TypeExpr> {
    let field_names = values_list_field_names(request);
    if bool_keyword_argument(&request.arguments, "named") == Some(true) {
        if field_names.is_empty() {
            model_fields(request, model_name)?;
            return Some(TypeExpr::annotation(
                model_values_list_row_virtual_type_name(model_name),
            ));
        }
        return values_list_named_row_type(request, model_name, &field_names);
    }

    if field_names.is_empty() {
        if bool_keyword_argument(&request.arguments, "flat") == Some(true) {
            return None;
        }
        let fields = model_fields(request, model_name)?;
        let row_types = fields
            .iter()
            .map(|(_, field_type)| field_type.as_str().unwrap_or("Any").to_string())
            .collect::<Vec<_>>();
        return Some(TypeExpr::annotation(format!(
            "tuple[{}]",
            row_types.join(", ")
        )));
    }

    if bool_keyword_argument(&request.arguments, "flat") == Some(true) {
        return Some(
            field_type_for_name(request, model_name, field_names[0])
                .unwrap_or_else(|| TypeExpr::annotation("str")),
        );
    }

    let fields = model_fields(request, model_name)?;
    let row_types = field_names
        .into_iter()
        .map(|field_name| {
            fields
                .get(field_name)
                .and_then(Value::as_str)
                .unwrap_or("Any")
                .to_string()
        })
        .collect::<Vec<_>>();
    Some(TypeExpr::annotation(format!(
        "tuple[{}]",
        row_types.join(", ")
    )))
}

fn values_list_named_row_type(
    request: &CallRequest,
    model_name: &str,
    field_names: &[&str],
) -> Option<TypeExpr> {
    let fields = model_fields(request, model_name)?;
    let entries = field_names
        .iter()
        .map(|field_name| {
            let field_type = fields
                .get(*field_name)
                .and_then(Value::as_str)
                .unwrap_or("object");
            format!("{}: {field_type}", json!(field_name))
        })
        .collect::<Vec<_>>();

    Some(TypeExpr::annotation(format!(
        r#"NamedTuple("MiniDjangoValuesListRow", {{{}}})"#,
        entries.join(", ")
    )))
}

fn values_row_type(request: &CallRequest, model_name: &str) -> Option<TypeExpr> {
    let fields = model_fields(request, model_name)?;
    let field_names = values_list_field_names(request);
    if field_names.is_empty() {
        return Some(TypeExpr::annotation(model_values_row_virtual_type_name(
            model_name,
        )));
    }

    let entries = field_names
        .into_iter()
        .map(|field_name| {
            let field_type = fields
                .get(field_name)
                .and_then(Value::as_str)
                .unwrap_or("object");
            format!("{}: {field_type}", json!(field_name))
        })
        .collect::<Vec<_>>();

    Some(TypeExpr::annotation(format!(
        "TypedDict({{{}}})",
        entries.join(", ")
    )))
}

fn values_list_field_names(request: &CallRequest) -> Vec<&str> {
    request
        .arguments
        .iter()
        .filter_map(|argument| {
            if argument.kind != ArgumentKind::Positional {
                return None;
            }
            let LiteralValue::Str { value } = &argument.value else {
                return None;
            };
            Some(value.as_str())
        })
        .collect()
}

fn field_type_for_name(
    request: &CallRequest,
    model_name: &str,
    field_name: &str,
) -> Option<TypeExpr> {
    model_fields(request, model_name)?
        .get(field_name)?
        .as_str()
        .map(TypeExpr::annotation)
}

fn validate_lookup_arguments(
    method_name: &str,
    model_name: &str,
    request: &CallRequest,
) -> Vec<PluginDiagnostic> {
    if model_fields(request, model_name).is_none() {
        return Vec::new();
    }

    match method_name {
        "filter" | "get" | "get_or_create" => request
            .arguments
            .iter()
            .filter(|argument| argument.kind == ArgumentKind::Keyword)
            .filter_map(|argument| validate_lookup_argument(model_name, request, argument))
            .collect(),
        "values" | "values_list" => request
            .arguments
            .iter()
            .filter(|argument| argument.kind == ArgumentKind::Positional)
            .filter_map(|argument| validate_values_list_argument(model_name, request, argument))
            .collect(),
        _ => Vec::new(),
    }
}

fn validate_lookup_argument(
    model_name: &str,
    request: &CallRequest,
    argument: &ArgumentSummary,
) -> Option<PluginDiagnostic> {
    let lookup = argument.name.as_deref()?;
    if !lookup_is_supported(request, model_name, lookup) {
        return Some(unknown_lookup_diagnostic(model_name, lookup, argument));
    }
    let (field_name, field_type, terminal_lookup) = lookup_field_type(request, model_name, lookup)?;
    if !lookup_value_is_compatible(field_type, terminal_lookup, argument) {
        return Some(invalid_lookup_value_diagnostic(
            model_name, field_name, lookup, field_type, argument,
        ));
    }
    None
}

fn validate_values_list_argument(
    model_name: &str,
    request: &CallRequest,
    argument: &ArgumentSummary,
) -> Option<PluginDiagnostic> {
    let fields = model_fields(request, model_name)?;
    let LiteralValue::Str { value } = &argument.value else {
        return None;
    };
    if !fields.contains_key(value) {
        return Some(unknown_lookup_diagnostic(model_name, value, argument));
    }
    None
}

fn lookup_is_supported(request: &CallRequest, model_name: &str, lookup: &str) -> bool {
    lookup_field_type(request, model_name, lookup).is_some()
}

fn lookup_field_type<'a>(
    request: &'a CallRequest,
    model_name: &str,
    lookup: &'a str,
) -> Option<(&'a str, &'a str, Option<&'a str>)> {
    let parts = lookup.split("__").collect::<Vec<_>>();
    if parts.is_empty() || parts.iter().any(|part| part.is_empty()) {
        return None;
    }

    let (field_path, terminal_lookup) = if parts
        .last()
        .is_some_and(|lookup_name| terminal_lookup_is_supported(lookup_name))
    {
        (&parts[..parts.len() - 1], parts.last().copied())
    } else {
        (parts.as_slice(), None)
    };

    field_path_type(request, model_name, field_path)
        .map(|(field_name, field_type)| (field_name, field_type, terminal_lookup))
}

fn terminal_lookup_is_supported(lookup_name: &str) -> bool {
    matches!(
        lookup_name,
        "exact"
            | "iexact"
            | "contains"
            | "icontains"
            | "startswith"
            | "istartswith"
            | "endswith"
            | "iendswith"
            | "regex"
            | "iregex"
            | "gt"
            | "gte"
            | "lt"
            | "lte"
            | "in"
            | "range"
            | "isnull"
    )
}

fn field_path_type<'a>(
    request: &'a CallRequest,
    model_name: &str,
    path: &[&'a str],
) -> Option<(&'a str, &'a str)> {
    let (last, prefix) = path.split_last()?;
    let mut current_model = model_name.to_string();
    for field_name in prefix {
        let field_type = model_fields(request, &current_model)
            .and_then(|fields| fields.get(*field_name))
            .and_then(Value::as_str)?;
        current_model = related_model_name(request, field_type)?;
    }

    model_fields(request, &current_model)
        .and_then(|fields| fields.get(*last))
        .and_then(Value::as_str)
        .map(|field_type| (*last, field_type))
}

fn lookup_value_is_compatible(
    field_type: &str,
    terminal_lookup: Option<&str>,
    argument: &ArgumentSummary,
) -> bool {
    let lookup = terminal_lookup.unwrap_or("exact");
    if lookup == "isnull" {
        matches!(argument.value, LiteralValue::Bool { .. })
    } else if lookup == "in" {
        match &argument.value {
            LiteralValue::List { items } | LiteralValue::Tuple { items } => items
                .iter()
                .all(|item| literal_value_matches_field_type(field_type, item)),
            LiteralValue::Unknown => true,
            _ => false,
        }
    } else if lookup == "range" {
        match &argument.value {
            LiteralValue::List { items } | LiteralValue::Tuple { items } if items.len() == 2 => {
                items
                    .iter()
                    .all(|item| literal_value_matches_field_type(field_type, item))
            }
            LiteralValue::Unknown => true,
            _ => false,
        }
    } else if matches!(
        lookup,
        "contains"
            | "icontains"
            | "startswith"
            | "istartswith"
            | "endswith"
            | "iendswith"
            | "regex"
            | "iregex"
    ) {
        field_type_allows(field_type, "str")
            && matches!(
                argument.value,
                LiteralValue::Str { .. } | LiteralValue::Unknown
            )
    } else {
        literal_value_matches_field_type(field_type, &argument.value)
    }
}

fn literal_value_matches_field_type(field_type: &str, value: &LiteralValue) -> bool {
    match value {
        LiteralValue::Unknown => true,
        LiteralValue::None => field_type_allows(field_type, "None"),
        LiteralValue::Bool { .. } => field_type_allows(field_type, "bool"),
        LiteralValue::Int { .. } => field_type_allows(field_type, "int"),
        LiteralValue::Str { .. } => field_type_allows(field_type, "str"),
        LiteralValue::ClassRef(_) | LiteralValue::EnumRef(_) | LiteralValue::SymbolRef(_) => true,
        LiteralValue::List { .. } | LiteralValue::Tuple { .. } | LiteralValue::Dict { .. } => false,
    }
}

fn field_type_allows(field_type: &str, expected: &str) -> bool {
    field_type
        .split('|')
        .map(str::trim)
        .any(|candidate| candidate == expected)
}

fn related_model_name(request: &CallRequest, field_type: &str) -> Option<String> {
    field_type
        .split('|')
        .map(str::trim)
        .filter(|candidate| *candidate != "None")
        .find(|candidate| model_fields(request, candidate).is_some())
        .map(ToString::to_string)
}

fn model_fields<'a>(
    request: &'a CallRequest,
    model_name: &str,
) -> Option<&'a serde_json::Map<String, Value>> {
    request
        .project_index
        .as_ref()?
        .get("models")?
        .get(model_name)?
        .get("fields")?
        .as_object()
}

fn unknown_lookup_diagnostic(
    model_name: &str,
    lookup: &str,
    argument: &ArgumentSummary,
) -> PluginDiagnostic {
    PluginDiagnostic {
        id: "minidjango.unknown-lookup".to_string(),
        message: format!("Unknown Mini-Django lookup `{lookup}` for model `{model_name}`"),
        severity: DiagnosticSeverity::Error,
        location: argument
            .source
            .as_ref()
            .and_then(diagnostic_location_from_source),
        metadata: BTreeMap::default(),
    }
}

fn invalid_lookup_value_diagnostic(
    model_name: &str,
    field_name: &str,
    lookup: &str,
    field_type: &str,
    argument: &ArgumentSummary,
) -> PluginDiagnostic {
    PluginDiagnostic {
        id: "minidjango.invalid-lookup-value".to_string(),
        message: format!(
            "Invalid Mini-Django lookup value for `{lookup}` on `{model_name}.{field_name}`; expected `{field_type}`"
        ),
        severity: DiagnosticSeverity::Error,
        location: argument
            .source
            .as_ref()
            .and_then(diagnostic_location_from_source),
        metadata: BTreeMap::default(),
    }
}

fn diagnostic_location_from_source(source: &SymbolSource) -> Option<DiagnosticLocation> {
    Some(DiagnosticLocation {
        file_path: source.file_path.clone()?,
        start: source.start?,
        end: source.end?,
    })
}

fn unknown_relation_target_diagnostic(
    model_name: &str,
    field_name: &str,
    target_name: &str,
    source: &SymbolSource,
) -> PluginDiagnostic {
    PluginDiagnostic {
        id: "minidjango.unknown-relation-target".to_string(),
        message: format!(
            "Unknown Mini-Django relation target `{target_name}` for field `{model_name}.{field_name}`"
        ),
        severity: DiagnosticSeverity::Error,
        location: diagnostic_location_from_source(source),
        metadata: BTreeMap::default(),
    }
}

fn reverse_relation_conflict_diagnostic(
    target_name: &str,
    reverse_name: &str,
    source: &SymbolSource,
    first_source: &SymbolSource,
) -> PluginDiagnostic {
    let mut metadata = BTreeMap::new();
    if let Some(file_path) = first_source.file_path.as_ref() {
        metadata.insert("first-file-path".to_string(), json!(file_path));
    }
    PluginDiagnostic {
        id: "minidjango.reverse-relation-conflict".to_string(),
        message: format!("Conflicting Mini-Django reverse relation `{target_name}.{reverse_name}`"),
        severity: DiagnosticSeverity::Error,
        location: diagnostic_location_from_source(source),
        metadata,
    }
}

fn derives_from_model(request: &AnalyzeClassRequest) -> bool {
    derives_from_model_class(&request.class)
}

fn derives_from_model_class(class: &ty_plugin_sdk::protocol::ClassSummary) -> bool {
    class.bases.iter().any(|base| base.expression == MODEL_BASE)
}

fn model_field_index(
    class: &ty_plugin_sdk::protocol::ClassSummary,
    settings_values: &BTreeMap<String, String>,
) -> serde_json::Map<String, Value> {
    let mut fields = serde_json::Map::new();
    fields.insert("id".to_string(), json!("int"));
    fields.insert("pk".to_string(), json!("int"));

    for field in &class.fields {
        let Some(call) = field_call(field.assigned_value.as_ref()) else {
            continue;
        };
        if is_manager_call(call) {
            continue;
        }
        let Some(field_type) = field_type_from_call(
            class_module_name(&class.qualified_name),
            &class.qualified_name,
            call,
            settings_values,
        ) else {
            continue;
        };
        fields.insert(field.name.clone(), json!(field_type.expression));
        if is_foreign_key_call(call) {
            fields.insert(
                format!("{}_id", field.name),
                json!(nullable_type("int", field_has_null_true(call)).expression),
            );
        }
    }

    fields
}

fn model_virtual_type_definitions(
    model_name: &str,
    fields: &serde_json::Map<String, Value>,
) -> Vec<VirtualTypeDefinition> {
    vec![
        VirtualTypeDefinition {
            name: model_manager_virtual_type_name(model_name),
            shape: VirtualTypeShape::Class {
                bases: vec![TypeExpr::expression(format!(
                    "{MANAGER_BASE}[{model_name}]"
                ))],
                members: Vec::new(),
            },
            metadata: Value::default(),
        },
        VirtualTypeDefinition {
            name: model_values_row_virtual_type_name(model_name),
            shape: VirtualTypeShape::TypedDict {
                fields: model_virtual_type_fields(fields),
                total: true,
            },
            metadata: Value::default(),
        },
        VirtualTypeDefinition {
            name: model_values_list_row_virtual_type_name(model_name),
            shape: VirtualTypeShape::NamedTuple {
                fields: model_virtual_type_fields(fields),
            },
            metadata: Value::default(),
        },
    ]
}

fn model_virtual_type_fields(fields: &serde_json::Map<String, Value>) -> Vec<VirtualTypeField> {
    fields
        .iter()
        .filter_map(|(name, field_type)| {
            Some(VirtualTypeField {
                name: name.clone(),
                type_expr: TypeExpr::annotation(field_type.as_str()?.to_string()),
                required: true,
                read_only: false,
            })
        })
        .collect()
}

fn model_values_row_virtual_type_name(model_name: &str) -> String {
    format!("minidjango.virtual.{model_name}.ValuesRow")
}

fn model_values_list_row_virtual_type_name(model_name: &str) -> String {
    format!("minidjango.virtual.{model_name}.ValuesListRow")
}

fn model_manager_virtual_type_name(model_name: &str) -> String {
    format!("minidjango.virtual.{model_name}.Manager")
}

fn settings_value_index(request: &BuildProjectIndexRequest) -> BTreeMap<String, String> {
    request
        .settings
        .iter()
        .flat_map(|settings| {
            settings.values.iter().filter_map(|value| {
                let LiteralValue::Str {
                    value: setting_value,
                } = &value.value
                else {
                    return None;
                };
                Some((
                    format!("{}.{}", settings.module, value.name),
                    setting_value.clone(),
                ))
            })
        })
        .collect()
}

fn settings_value_index_from_project_index(
    project_index: Option<&Value>,
) -> BTreeMap<String, String> {
    project_index
        .and_then(|index| index.get("settings"))
        .and_then(Value::as_object)
        .map(|settings| {
            settings
                .iter()
                .filter_map(|(key, value)| Some((key.clone(), value.as_str()?.to_string())))
                .collect()
        })
        .unwrap_or_default()
}

fn class_module_name(qualified_name: &str) -> &str {
    qualified_name
        .rsplit_once('.')
        .map_or("", |(module, _)| module)
}

fn reverse_relation_name(qualified_name: &str, call: &CallValueSummary) -> Option<String> {
    if let Some(related_name) = string_keyword_argument(call, "related_name") {
        if related_name == "+" {
            return None;
        }
        return Some(related_name.to_string());
    }

    let class_name = qualified_name
        .rsplit('.')
        .next()
        .unwrap_or(qualified_name)
        .to_ascii_lowercase();
    Some(format!("{class_name}_set"))
}

fn field_call(assigned_value: Option<&AssignedValueSummary>) -> Option<&CallValueSummary> {
    let Some(AssignedValueSummary::Call(call)) = assigned_value else {
        return None;
    };
    Some(call)
}

fn field_patch_from_call(
    request: &AnalyzeClassRequest,
    field_name: &str,
    call: &CallValueSummary,
    settings_values: &BTreeMap<String, String>,
) -> Option<FieldPatch> {
    let ty = field_type_from_call(
        &request.context.module,
        &request.class.qualified_name,
        call,
        settings_values,
    )?;

    let parameter = dsl::keyword_only(field_name, ty.clone());
    let parameter = if field_has_null_true(call) {
        dsl::optional(parameter)
    } else {
        parameter
    };

    Some(FieldPatch {
        mode: MemberPatchMode::ReplaceExisting,
        name: field_name.to_string(),
        descriptor: Some(MemberAccessPatch::Descriptor {
            class_type: None,
            instance_get_type: ty.clone(),
            instance_set_type: Some(ty.clone()),
        }),
        instance_get_type: ty.clone(),
        instance_set_type: Some(ty),
        constructor_parameter: Some(parameter),
        has_default: false,
    })
}

fn field_type_from_call(
    module: &str,
    class_qualified_name: &str,
    call: &CallValueSummary,
    settings_values: &BTreeMap<String, String>,
) -> Option<TypeExpr> {
    if callee_matches(call, "CharField") {
        Some(nullable_type("str", field_has_null_true(call)))
    } else if callee_matches(call, "IntegerField") {
        Some(nullable_type("int", field_has_null_true(call)))
    } else if is_foreign_key_call(call) {
        Some(nullable_type(
            relation_target_type(module, class_qualified_name, call, settings_values)?.expression,
            field_has_null_true(call),
        ))
    } else {
        None
    }
}

fn non_init_field(name: impl Into<String>, ty: TypeExpr) -> FieldPatch {
    FieldPatch {
        mode: MemberPatchMode::FillOnMiss,
        name: name.into(),
        descriptor: None,
        instance_get_type: ty.clone(),
        instance_set_type: Some(ty),
        constructor_parameter: None,
        has_default: true,
    }
}

fn foreign_key_id_field(field_name: &str, nullable: bool) -> FieldPatch {
    let ty = nullable_type("int", nullable);
    non_init_field(format!("{field_name}_id"), ty)
}

fn callee_matches(call: &CallValueSummary, name: &str) -> bool {
    call.callee
        .qualified_name
        .rsplit('.')
        .next()
        .is_some_and(|last| last == name)
}

fn is_foreign_key_call(call: &CallValueSummary) -> bool {
    callee_matches(call, "ForeignKey")
}

fn is_manager_call(call: &CallValueSummary) -> bool {
    call.callee
        .qualified_name
        .rsplit('.')
        .next()
        .is_some_and(|last| last == "Manager" || last.ends_with("Manager"))
}

fn field_has_null_true(call: &CallValueSummary) -> bool {
    call.arguments.iter().any(|argument| {
        argument.name.as_deref() == Some("null")
            && matches!(argument.value, LiteralValue::Bool { value: true })
    })
}

fn string_keyword_argument<'a>(call: &'a CallValueSummary, name: &str) -> Option<&'a str> {
    call.arguments.iter().find_map(|argument| {
        if argument.name.as_deref() != Some(name) {
            return None;
        }
        let LiteralValue::Str { value } = &argument.value else {
            return None;
        };
        Some(value.as_str())
    })
}

fn bool_keyword_argument(arguments: &[ArgumentSummary], name: &str) -> Option<bool> {
    arguments.iter().find_map(|argument| {
        if argument.name.as_deref() != Some(name) || argument.kind != ArgumentKind::Keyword {
            return None;
        }
        let LiteralValue::Bool { value } = &argument.value else {
            return None;
        };
        Some(*value)
    })
}

fn nullable_type(expression: impl Into<String>, nullable: bool) -> TypeExpr {
    let expression = expression.into();
    if nullable {
        TypeExpr::annotation(format!("{expression} | None"))
    } else {
        TypeExpr::annotation(expression)
    }
}

fn relation_target_type(
    module: &str,
    class_qualified_name: &str,
    call: &CallValueSummary,
    settings_values: &BTreeMap<String, String>,
) -> Option<TypeExpr> {
    let target = call
        .arguments
        .iter()
        .find(|argument| argument.kind == ArgumentKind::Positional)?;

    match &target.value {
        LiteralValue::EnumRef(symbol) | LiteralValue::SymbolRef(symbol)
            if settings_values.contains_key(&symbol.qualified_name) =>
        {
            settings_values
                .get(&symbol.qualified_name)
                .map(|value| TypeExpr::annotation(value.clone()))
        }
        LiteralValue::ClassRef(symbol) | LiteralValue::SymbolRef(symbol) => target
            .type_expr
            .clone()
            .or_else(|| Some(TypeExpr::annotation(symbol.qualified_name.clone()))),
        LiteralValue::EnumRef(symbol) => target
            .type_expr
            .clone()
            .or_else(|| Some(TypeExpr::annotation(symbol.qualified_name.clone()))),
        LiteralValue::Str { value } if value == "self" => {
            Some(TypeExpr::annotation(class_qualified_name.to_string()))
        }
        LiteralValue::Str { value } if settings_values.contains_key(value) => settings_values
            .get(value)
            .map(|value| TypeExpr::annotation(value.clone())),
        LiteralValue::Str { value } if value.contains('.') => Some(TypeExpr::annotation(value)),
        LiteralValue::Str { value } => Some(TypeExpr::annotation(format!("{module}.{value}"))),
        _ => target.type_expr.clone(),
    }
}
