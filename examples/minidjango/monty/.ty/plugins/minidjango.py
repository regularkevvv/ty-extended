"""A Mini-Django semantic plugin, authored in Python.

Port of the reference `MiniDjangoPlugin` (Rust -> WASM) to `ty.plugin_sdk`: same
claims, same responses, no Rust toolchain. See the example README for what each
hook implements.
"""

from __future__ import annotations

import json
import typing

if typing.TYPE_CHECKING:
    # IDE-only import; inside Monty these names are ambient.
    from ty.plugin_sdk import (
        Request,
        Response,
        attribute_claim_contribution_target,
        call_arguments,
        call_return_patch,
        callee,
        capabilities,
        class_claim_subclass_of,
        class_patch,
        class_summary,
        claims,
        context,
        contribution,
        diagnostic,
        field_contribution,
        field_patch,
        instance_target,
        keyword_arguments,
        location,
        manifest,
        member,
        member_descriptor,
        method_claim_on_subclass_of,
        on_call_return,
        on_class_transform,
        on_project_index,
        parameter,
        project_index,
        project_index_of,
        receiver,
        settings_claim,
        set_manifest,
        type_annotation,
        type_expr,
        virtual_class,
        virtual_field,
        virtual_named_tuple,
        virtual_typed_dict,
        virtual_type,
    )

MODEL_BASE = "minidjango.Model"
MANAGER_BASE = "minidjango.Manager"
QUERYSET_BASE = "minidjango.QuerySet"
SETTINGS_MODULE = "minidjango_settings"

LOOKUP_METHODS = (
    "filter",
    "get",
    "get_or_create",
    "first",
    "count",
    "exists",
    "values",
    "values_list",
    "annotate",
)

TERMINAL_LOOKUPS = (
    "exact",
    "iexact",
    "contains",
    "icontains",
    "startswith",
    "istartswith",
    "endswith",
    "iendswith",
    "regex",
    "iregex",
    "gt",
    "gte",
    "lt",
    "lte",
    "in",
    "range",
    "isnull",
)

STR_TERMINAL_LOOKUPS = (
    "contains",
    "icontains",
    "startswith",
    "istartswith",
    "endswith",
    "iendswith",
    "regex",
    "iregex",
)

set_manifest(
    manifest(
        id="example.minidjango",
        name="Mini-Django proof plugin",
        version="0.1.0",
        capabilities=capabilities(
            class_transform=True,
            call_return=True,
            project_index=True,
            cross_symbol_contributions=True,
            settings_data=True,
            virtual_types=True,
        ),
        claims=claims(
            classes=[class_claim_subclass_of(MODEL_BASE)],
            attributes=[attribute_claim_contribution_target(MODEL_BASE, "instance")],
            methods=[
                method_claim_on_subclass_of(base, method)
                for base in (MANAGER_BASE, QUERYSET_BASE)
                for method in LOOKUP_METHODS
            ],
            settings=[settings_claim(module=SETTINGS_MODULE)],
        ),
        runtime={"kind": "monty", "artifact": "minidjango.py"},
    )
)


# Literal / call-summary helpers



def _field_call(assigned_value):
    if isinstance(assigned_value, dict) and assigned_value.get("kind") == "call":
        return assigned_value
    return None


def _call_name(call):
    qn = (call.get("callee") or {}).get("qualified-name") or ""
    return qn.rsplit(".", 1)[-1]


def _callee_matches(call, name):
    return _call_name(call) == name


def _is_foreign_key_call(call):
    return _callee_matches(call, "ForeignKey")


def _is_manager_call(call):
    return _call_name(call).endswith("Manager")


def _field_has_null_true(call):
    for argument in call.get("arguments") or []:
        value = argument.get("value") or {}
        if argument.get("name") == "null" and value.get("kind") == "bool" and value.get("value") is True:
            return True
    return False


def _string_keyword_argument(call, name):
    for argument in call.get("arguments") or []:
        value = argument.get("value") or {}
        if argument.get("name") == name and value.get("kind") == "str":
            return value.get("value")
    return None


def _bool_keyword_argument(arguments, name):
    for argument in arguments:
        value = argument.get("value") or {}
        if (
            argument.get("name") == name
            and argument.get("kind") == "keyword"
            and value.get("kind") == "bool"
        ):
            return value.get("value")
    return None


def _literal_str_values(arguments):
    names = []
    for argument in arguments:
        value = argument.get("value") or {}
        if argument.get("kind") == "positional" and value.get("kind") == "str":
            names.append(value.get("value"))
    return names


def _nullable_type(expression, nullable):
    return type_annotation(expression + " | None" if nullable else expression)


def _class_module_name(qualified_name):
    return qualified_name.rsplit(".", 1)[0] if "." in qualified_name else ""


def _derives_from_model(summary):
    return any(base.get("expression") == MODEL_BASE for base in summary.get("bases") or [])


# Settings values (claimed `minidjango_settings` constants)



def _settings_value_index(request):
    values = {}
    for module in request.get("settings") or []:
        for entry in module.get("values") or []:
            value = entry.get("value") or {}
            if value.get("kind") == "str":
                values[(module.get("module") or "") + "." + (entry.get("name") or "")] = value.get(
                    "value"
                )
    return values


def _settings_value_index_from_project_index(index):
    settings = (index or {}).get("settings")
    if not isinstance(settings, dict):
        return {}
    return {key: value for key, value in settings.items() if isinstance(value, str)}


# Relation targets and reverse relation names



def _relation_target_type(module, class_qualified_name, call, settings_values):
    target = None
    for argument in call.get("arguments") or []:
        if argument.get("kind") == "positional":
            target = argument
            break
    if target is None:
        return None

    value = target.get("value") or {}
    kind = value.get("kind")
    qualified_name = value.get("qualified-name")

    if kind in ("enum-ref", "symbol-ref") and qualified_name in settings_values:
        return type_annotation(settings_values[qualified_name])
    if kind in ("class-ref", "symbol-ref", "enum-ref"):
        return target.get("type-expr") or (
            type_annotation(qualified_name) if isinstance(qualified_name, str) else None
        )
    if kind == "str":
        text = value.get("value")
        if not isinstance(text, str):
            return target.get("type-expr")
        if text == "self":
            return type_annotation(class_qualified_name)
        if text in settings_values:
            return type_annotation(settings_values[text])
        if "." in text:
            return type_annotation(text)
        return type_annotation(module + "." + text)
    return target.get("type-expr")


def _reverse_relation_name(class_qualified_name, call):
    related_name = _string_keyword_argument(call, "related_name")
    if related_name is not None:
        return None if related_name == "+" else related_name
    class_name = class_qualified_name.rsplit(".", 1)[-1]
    return class_name.lower() + "_set"


# Model field index and virtual types



def _manager_virtual_type_name(model_name):
    return "minidjango.virtual." + model_name + ".Manager"


def _values_row_virtual_type_name(model_name):
    return "minidjango.virtual." + model_name + ".ValuesRow"


def _values_list_row_virtual_type_name(model_name):
    return "minidjango.virtual." + model_name + ".ValuesListRow"


def _field_type_from_call(module, class_qualified_name, call, settings_values):
    nullable = _field_has_null_true(call)
    if _callee_matches(call, "CharField"):
        return _nullable_type("str", nullable)
    if _callee_matches(call, "IntegerField"):
        return _nullable_type("int", nullable)
    if _is_foreign_key_call(call):
        target = _relation_target_type(module, class_qualified_name, call, settings_values)
        if target is None:
            return None
        return _nullable_type(target.get("expression"), nullable)
    return None


def _model_field_index(summary, settings_values):
    fields = {"id": "int", "pk": "int"}
    for field_summary in summary.get("fields") or []:
        call = _field_call(field_summary.get("assigned-value"))
        if call is None or _is_manager_call(call):
            continue
        field_type = _field_type_from_call(
            _class_module_name(summary.get("qualified-name") or ""),
            summary.get("qualified-name") or "",
            call,
            settings_values,
        )
        if field_type is None:
            continue
        name = field_summary.get("name")
        fields[name] = field_type.get("expression")
        if _is_foreign_key_call(call):
            fields[name + "_id"] = _nullable_type("int", _field_has_null_true(call)).get(
                "expression"
            )
    return fields


def _model_virtual_type_fields(fields):
    return [virtual_field(name, type_annotation(field_type)) for name, field_type in fields.items()]


def _model_virtual_type_definitions(model_name, fields):
    return [
        virtual_type(
            _manager_virtual_type_name(model_name),
            virtual_class(bases=[type_expr(MANAGER_BASE + "[" + model_name + "]")]),
        ),
        virtual_type(
            _values_row_virtual_type_name(model_name),
            virtual_typed_dict(fields=_model_virtual_type_fields(fields)),
        ),
        virtual_type(
            _values_list_row_virtual_type_name(model_name),
            virtual_named_tuple(fields=_model_virtual_type_fields(fields)),
        ),
    ]


def _model_fields(request, model_name):
    models = project_index_of(request).get("models")
    if not isinstance(models, dict):
        return None
    fields = (models.get(model_name) or {}).get("fields")
    return fields if isinstance(fields, dict) else None


# Diagnostics



def _diagnostic_location_from_source(source):
    if not isinstance(source, dict):
        return None
    file_path = source.get("file-path")
    start = source.get("start")
    end = source.get("end")
    if file_path is None or start is None or end is None:
        return None
    return location(file_path, start, end)


def _unknown_lookup_diagnostic(model_name, lookup, argument):
    return diagnostic(
        "minidjango.unknown-lookup",
        "Unknown Mini-Django lookup `" + lookup + "` for model `" + model_name + "`",
        severity="error",
        location=_diagnostic_location_from_source(argument.get("source")),
    )


def _invalid_lookup_value_diagnostic(model_name, field_name, lookup, field_type, argument):
    return diagnostic(
        "minidjango.invalid-lookup-value",
        "Invalid Mini-Django lookup value for `"
        + lookup
        + "` on `"
        + model_name
        + "."
        + field_name
        + "`; expected `"
        + field_type
        + "`",
        severity="error",
        location=_diagnostic_location_from_source(argument.get("source")),
    )


def _unknown_relation_target_diagnostic(model_name, field_name, target_name, source):
    return diagnostic(
        "minidjango.unknown-relation-target",
        "Unknown Mini-Django relation target `"
        + target_name
        + "` for field `"
        + model_name
        + "."
        + field_name
        + "`",
        severity="error",
        location=_diagnostic_location_from_source(source),
    )


def _reverse_relation_conflict_diagnostic(target_name, reverse_name, source, first_source):
    metadata = {}
    first_path = (first_source or {}).get("file-path")
    if first_path is not None:
        metadata["first-file-path"] = first_path
    return diagnostic(
        "minidjango.reverse-relation-conflict",
        "Conflicting Mini-Django reverse relation `" + target_name + "." + reverse_name + "`",
        severity="error",
        location=_diagnostic_location_from_source(source),
        metadata=metadata,
    )


# build-project-index



@on_project_index
def build_index(request: Request) -> Response:
    settings_values = _settings_value_index(request)
    classes = request.get("classes") or []
    model_names = {_class.get("qualified-name") for _class in classes if _derives_from_model(_class)}

    contributions = []
    diagnostics = []
    virtual_types = []
    models = {}
    reverse_names = {}

    for summary in classes:
        if not _derives_from_model(summary):
            continue
        qualified_name = summary.get("qualified-name") or ""
        fields = _model_field_index(summary, settings_values)
        virtual_types.extend(_model_virtual_type_definitions(qualified_name, fields))
        models[qualified_name] = {"fields": fields}

        for field_summary in summary.get("fields") or []:
            call = _field_call(field_summary.get("assigned-value"))
            if call is None or not _is_foreign_key_call(call):
                continue
            source = field_summary.get("source") or {}
            target = _relation_target_type(
                _class_module_name(qualified_name), qualified_name, call, settings_values
            )
            if target is None:
                continue
            target_name = target.get("expression")
            if target_name not in model_names:
                diagnostics.append(
                    _unknown_relation_target_diagnostic(
                        qualified_name, field_summary.get("name"), target_name, source
                    )
                )
                continue
            reverse_name = _reverse_relation_name(qualified_name, call)
            if reverse_name is None:
                continue
            conflict_key = target_name + "." + reverse_name
            first_source = reverse_names.get(conflict_key)
            if first_source is not None:
                diagnostics.append(
                    _reverse_relation_conflict_diagnostic(
                        target_name, reverse_name, source, first_source
                    )
                )
                continue
            reverse_names[conflict_key] = source
            contributions.append(
                contribution(
                    source=source,
                    target=instance_target(target_name),
                    patch=field_contribution(
                        field_patch(
                            reverse_name,
                            type_annotation(_manager_virtual_type_name(qualified_name)),
                            mode="fill-on-miss",
                            has_default=True,
                        )
                    ),
                    conflict_key=conflict_key,
                )
            )

    return project_index(
        plugin_index={"models": models, "settings": settings_values},
        contributions=contributions,
        virtual_types=virtual_types,
        diagnostics=diagnostics,
    )


# analyze-class



def _non_init_field(name, field_type):
    return field_patch(
        name,
        field_type,
        mode="fill-on-miss",
        instance_set_type=field_type,
        has_default=True,
    )


def _foreign_key_id_field(field_name, nullable):
    return _non_init_field(field_name + "_id", _nullable_type("int", nullable))


def _field_patch_from_call(request, field_name, call, settings_values):
    field_type = _field_type_from_call(
        context(request).get("module") or "",
        class_summary(request).get("qualified-name") or "",
        call,
        settings_values,
    )
    if field_type is None:
        return None
    has_default = _field_has_null_true(call)
    return field_patch(
        field_name,
        field_type,
        mode="replace-existing",
        descriptor=member_descriptor(field_type, None, field_type),
        instance_set_type=field_type,
        constructor_parameter=parameter(
            name=field_name, kind="keyword-only", type=field_type, required=not has_default
        ),
        has_default=False,
    )


@on_class_transform
def transform_model(request: Request) -> Response:
    summary = class_summary(request)
    if not _derives_from_model(summary):
        return None

    qualified_name = summary.get("qualified-name") or ""
    manager_type = type_annotation(_manager_virtual_type_name(qualified_name))
    settings_values = _settings_value_index_from_project_index(project_index_of(request))

    fields = [
        _non_init_field("id", type_annotation("int")),
        _non_init_field("pk", type_annotation("int")),
    ]
    class_members = [
        member("objects", manager_type),
        member("_default_manager", manager_type),
    ]

    for field_summary in summary.get("fields") or []:
        call = _field_call(field_summary.get("assigned-value"))
        if call is None:
            continue
        if _is_manager_call(call):
            class_members.append(member(field_summary.get("name"), manager_type))
            continue
        patch = _field_patch_from_call(request, field_summary.get("name"), call, settings_values)
        if patch is None:
            continue
        has_default = bool(field_summary.get("has-default")) or _field_has_null_true(call)
        if has_default:
            patch["has-default"] = True
            if patch.get("constructor-parameter") is not None:
                patch["constructor-parameter"]["required"] = False
        if _is_foreign_key_call(call):
            fields.append(
                _foreign_key_id_field(field_summary.get("name"), _field_has_null_true(call))
            )
        fields.append(patch)

    return class_patch(fields=fields, class_members=class_members)


# adjust-call-return



def _queryset_type(model_type, row_type):
    return type_annotation(
        QUERYSET_BASE + "[" + model_type.get("expression") + ", " + row_type.get("expression") + "]"
    )


def _annotation_argument_type(argument):
    if argument.get("type-expr") is not None:
        return argument.get("type-expr")
    kind = (argument.get("value") or {}).get("kind")
    if kind == "bool":
        return type_annotation("bool")
    if kind == "int":
        return type_annotation("int")
    if kind == "str":
        return type_annotation("str")
    if kind == "none":
        return type_annotation("None")
    return type_annotation("object")


def _annotated_row_type(request, base_row_type):
    entries = []
    for name, argument in keyword_arguments(request).items():
        entries.append(json.dumps(name) + ": " + _annotation_argument_type(argument).get("expression"))
    if not entries:
        return None
    return type_annotation(
        'Class("MiniDjangoAnnotatedRow", {' + ", ".join(entries) + "}, "
        + base_row_type.get("expression") + ")"
    )


def _values_list_field_names(request):
    return _literal_str_values(call_arguments(request))


def _values_list_named_row_type(request, model_name, field_names):
    fields = _model_fields(request, model_name)
    if fields is None:
        return None
    entries = [json.dumps(name) + ": " + fields.get(name, "object") for name in field_names]
    return type_annotation('NamedTuple("MiniDjangoValuesListRow", {' + ", ".join(entries) + "})")


def _values_row_type(request, model_name):
    fields = _model_fields(request, model_name)
    if fields is None:
        return None
    field_names = _values_list_field_names(request)
    if not field_names:
        return type_annotation(_values_row_virtual_type_name(model_name))
    entries = [json.dumps(name) + ": " + fields.get(name, "object") for name in field_names]
    return type_annotation("TypedDict({" + ", ".join(entries) + "})")


def _field_type_for_name(request, model_name, field_name):
    fields = _model_fields(request, model_name)
    if fields is None or field_name not in fields:
        return None
    return type_annotation(fields[field_name])


def _values_list_row_type(request, model_name):
    field_names = _values_list_field_names(request)
    named = _bool_keyword_argument(call_arguments(request), "named") is True
    flat = _bool_keyword_argument(call_arguments(request), "flat") is True

    if named:
        if not field_names:
            if _model_fields(request, model_name) is None:
                return None
            return type_annotation(_values_list_row_virtual_type_name(model_name))
        return _values_list_named_row_type(request, model_name, field_names)

    if not field_names:
        if flat:
            return None
        fields = _model_fields(request, model_name)
        if fields is None:
            return None
        return type_annotation("tuple[" + ", ".join(fields.values()) + "]")

    if flat:
        return _field_type_for_name(request, model_name, field_names[0]) or type_annotation("str")

    fields = _model_fields(request, model_name)
    if fields is None:
        return None
    return type_annotation(
        "tuple[" + ", ".join(fields.get(name, "Any") for name in field_names) + "]"
    )


# Lookup validation



def _field_type_allows(field_type, expected):
    return any(candidate.strip() == expected for candidate in field_type.split("|"))


def _literal_value_matches_field_type(field_type, value):
    if not isinstance(value, dict):
        return True
    kind = value.get("kind")
    if kind == "unknown" or kind is None:
        return True
    if kind == "none":
        return _field_type_allows(field_type, "None")
    if kind == "bool":
        return _field_type_allows(field_type, "bool")
    if kind == "int":
        return _field_type_allows(field_type, "int")
    if kind == "str":
        return _field_type_allows(field_type, "str")
    if kind in ("class-ref", "enum-ref", "symbol-ref"):
        return True
    return False


def _related_model_name(request, field_type):
    for candidate in field_type.split("|"):
        candidate = candidate.strip()
        if candidate != "None" and _model_fields(request, candidate) is not None:
            return candidate
    return None


def _field_path_type(request, model_name, path):
    if not path:
        return None
    current_model = model_name
    for field_name in path[:-1]:
        fields = _model_fields(request, current_model)
        field_type = (fields or {}).get(field_name)
        if field_type is None:
            return None
        current_model = _related_model_name(request, field_type)
        if current_model is None:
            return None
    fields = _model_fields(request, current_model)
    field_type = (fields or {}).get(path[-1])
    if field_type is None:
        return None
    return (path[-1], field_type)


def _lookup_field_type(request, model_name, lookup):
    parts = lookup.split("__")
    if not parts or any(part == "" for part in parts):
        return None
    if parts[-1] in TERMINAL_LOOKUPS:
        field_path, terminal_lookup = parts[:-1], parts[-1]
    else:
        field_path, terminal_lookup = parts, None
    result = _field_path_type(request, model_name, field_path)
    if result is None:
        return None
    return (result[0], result[1], terminal_lookup)


def _lookup_value_is_compatible(field_type, terminal_lookup, argument):
    lookup = terminal_lookup or "exact"
    value = argument.get("value")
    if lookup == "isnull":
        return isinstance(value, dict) and value.get("kind") == "bool"
    if lookup == "in":
        if not isinstance(value, dict):
            return True
        if value.get("kind") in ("list", "tuple"):
            return all(
                _literal_value_matches_field_type(field_type, item)
                for item in value.get("items") or []
            )
        return value.get("kind") == "unknown" or value.get("kind") is None
    if lookup == "range":
        if not isinstance(value, dict):
            return True
        items = value.get("items")
        if value.get("kind") in ("list", "tuple") and isinstance(items, list) and len(items) == 2:
            return all(_literal_value_matches_field_type(field_type, item) for item in items)
        return value.get("kind") == "unknown" or value.get("kind") is None
    if lookup in STR_TERMINAL_LOOKUPS:
        return _field_type_allows(field_type, "str") and (
            not isinstance(value, dict)
            or value.get("kind") in ("str", "unknown")
            or value.get("kind") is None
        )
    return _literal_value_matches_field_type(field_type, value)


def _validate_lookup_argument(model_name, request, argument):
    lookup = argument.get("name")
    if lookup is None:
        return None
    result = _lookup_field_type(request, model_name, lookup)
    if result is None:
        return _unknown_lookup_diagnostic(model_name, lookup, argument)
    field_name, field_type, terminal_lookup = result
    if not _lookup_value_is_compatible(field_type, terminal_lookup, argument):
        return _invalid_lookup_value_diagnostic(
            model_name, field_name, lookup, field_type, argument
        )
    return None


def _validate_values_list_argument(model_name, request, argument):
    fields = _model_fields(request, model_name)
    if fields is None:
        return None
    value = argument.get("value") or {}
    if value.get("kind") != "str":
        return None
    if value.get("value") not in fields:
        return _unknown_lookup_diagnostic(model_name, value.get("value"), argument)
    return None


def _validate_lookup_arguments(method_name, model_name, request):
    if _model_fields(request, model_name) is None:
        return []
    arguments = call_arguments(request)
    diagnostics = []
    if method_name in ("filter", "get", "get_or_create"):
        for argument in arguments:
            if argument.get("kind") != "keyword":
                continue
            diag = _validate_lookup_argument(model_name, request, argument)
            if diag is not None:
                diagnostics.append(diag)
    elif method_name in ("values", "values_list"):
        for argument in arguments:
            if argument.get("kind") != "positional":
                continue
            diag = _validate_values_list_argument(model_name, request, argument)
            if diag is not None:
                diagnostics.append(diag)
    return diagnostics


@on_call_return
def adjust_return(request: Request) -> Response:
    recv = receiver(request)
    if recv is None:
        return None
    nominal = recv.get("nominal-class")
    if nominal not in (MANAGER_BASE, QUERYSET_BASE):
        return None

    method_name = (callee(request) or "").rsplit(".", 1)[-1]
    generic_arguments = recv.get("generic-arguments") or []
    if not generic_arguments:
        return None
    model_type = generic_arguments[0]
    is_queryset = nominal == QUERYSET_BASE
    row_type = generic_arguments[1] if is_queryset and len(generic_arguments) > 1 else model_type
    model_name = model_type.get("expression")

    diagnostics = _validate_lookup_arguments(method_name, model_name, request)

    if method_name == "filter":
        return call_return_patch(_queryset_type(model_type, row_type), diagnostics=diagnostics)
    if method_name == "get":
        return call_return_patch(row_type if is_queryset else model_type, diagnostics=diagnostics)
    if method_name == "get_or_create":
        return call_return_patch(
            type_annotation("tuple[" + model_name + ", bool]"), diagnostics=diagnostics
        )
    if method_name == "first":
        base = row_type if is_queryset else model_type
        return call_return_patch(
            type_annotation(base.get("expression") + " | None"), diagnostics=diagnostics
        )
    if method_name == "count":
        return call_return_patch(type_annotation("int"), diagnostics=diagnostics)
    if method_name == "exists":
        return call_return_patch(type_annotation("bool"), diagnostics=diagnostics)
    if method_name == "values":
        row = _values_row_type(request, model_name) or type_annotation("dict[str, object]")
        return call_return_patch(_queryset_type(model_type, row), diagnostics=diagnostics)
    if method_name == "values_list":
        row = _values_list_row_type(request, model_name)
        if row is None:
            return None
        return call_return_patch(_queryset_type(model_type, row), diagnostics=diagnostics)
    if method_name == "annotate":
        row = _annotated_row_type(request, row_type) or row_type
        return call_return_patch(_queryset_type(model_type, row), diagnostics=diagnostics)
    return None
